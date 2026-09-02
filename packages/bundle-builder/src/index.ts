import { createHash } from "node:crypto";
import { access, lstat, mkdtemp, open, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { tmpdir } from "node:os";
import { transform } from "oxc-transform";
import { rolldown } from "rolldown";
import { parseAst } from "rolldown/parseAst";

import { encodeBundleArchive } from "./archive.js";

export { encodeBundleArchive } from "./archive.js";
export type { BundleArchiveEntries, BundleArchiveEntryName } from "./archive.js";

export interface BundleOptions {
  readonly rootDir: string;
  readonly compilerVersion?: string;
  readonly bundlerVersion?: string;
}
export interface FileIntegrity { readonly path: string; readonly length: number; readonly sha256: string; }
export interface BundleIntegrityManifestV0 { readonly manifestVersion: string; readonly compiler: { identity: string; version: string }; readonly bundler: { identity: string; version: string }; readonly rendererAbiVersion: string; readonly descriptor: FileIntegrity; readonly renderModules: Array<FileIntegrity & { identity: string }>; readonly actionModules: Array<FileIntegrity & { identity: string }>; readonly sourceMaps: FileIntegrity[]; readonly phaseModules: Array<{ phase: string; moduleIdentity: string }>; }
export interface BuiltBundle { readonly bundleDigest: string; readonly manifest: BundleIntegrityManifestV0; readonly archiveBytes: Buffer; }

const ABI = "renderer.oxideloop.dev/v0";
const RENDER_IDENTITY = "controller.render";

interface AstNode {
  readonly type?: string;
  readonly start?: number;
  readonly end?: number;
  readonly [key: string]: unknown;
}

interface ControllerCompilation {
  readonly source: string;
  readonly controller: AstNode;
  readonly actionName: string;
  readonly actionIdentity: string;
}

const AUTHORED_IMPORTS = new Set([
  "ConfigMap", "Container", "Data", "Deployment", "Env", "Event", "Port", "Resources", "Service",
  "defineAction", "defineController", "useAutomation", "useEnrichment", "useResource",
]);
const ACTION_COMPONENT_IMPORTS = new Set(["Event"]);
const RENDER_CAPABILITIES = new Set(["useResource", "useEnrichment", "useAutomation"]);

/**
 * Build in a private staging directory and return one archive snapshot. Local
 * execution trusts this process and its private staging; the untrusted
 * boundary is the archive bytes later consumed by Rust.
 */
export async function buildBundle(options: BundleOptions): Promise<BuiltBundle> {
  const rootDir = resolve(options.rootDir);
  const workspaceRoot = await findWorkspaceRoot(rootDir);
  const controller = await validateAuthoredSources(rootDir);
  let staging: string | undefined;
  try {
    staging = await mkdtemp(join(tmpdir(), "oxideloop-bundle-"));
    const actionPath = join(staging, "action.callable.js");
    await bundle(
      join(rootDir, "controller.tsx"),
      actionModuleSource(controller),
      actionPath,
      "OxideLoopAction",
      "action",
      workspaceRoot,
      rootDir,
    );
    const actionFile = await integrity(actionPath, staging);

    // The action artifact is complete before its raw digest is compiled into
    // render. The final bundle digest is *not* compiled into render bytes:
    // the renderer receives it from the canonical RenderInputV0 at invocation.
    const renderPath = join(staging, "render.callable.js");
    await bundle(
      join(rootDir, "controller.tsx"),
      renderModuleSource(controller, actionFile.sha256),
      renderPath,
      "OxideLoopRender",
      "render",
      workspaceRoot,
      rootDir,
      { __OXIDELOOP_ACTION_MODULE_DIGEST__: JSON.stringify(actionFile.sha256) },
    );

    const descriptorPath = join(staging, "descriptor.json");
    const descriptor = {
      descriptorVersion: "oxideloop.dev/descriptor/v0",
      rendererAbiVersion: ABI,
      renderModule: { identity: RENDER_IDENTITY, digest: "" },
      actionModules: [{ identity: controller.actionIdentity, digest: actionFile.sha256 }],
    };
    const renderFile = await integrity(renderPath, staging);
    descriptor.renderModule.digest = renderFile.sha256;
    await writeFile(descriptorPath, `${JSON.stringify(descriptor, null, 2)}\n`, "utf8");

    const manifest: BundleIntegrityManifestV0 = {
      manifestVersion: "bundle.oxideloop.dev/v0",
      compiler: { identity: "oxc", version: options.compilerVersion ?? "0.58.0" },
      bundler: { identity: "rolldown", version: options.bundlerVersion ?? "1.0.0-beta.35" },
      rendererAbiVersion: ABI,
      descriptor: await integrity(descriptorPath, staging),
      renderModules: [{ identity: RENDER_IDENTITY, ...renderFile }],
      actionModules: [{ identity: controller.actionIdentity, ...actionFile }],
      sourceMaps: [],
      phaseModules: [
        { phase: "render", moduleIdentity: RENDER_IDENTITY },
        { phase: "action", moduleIdentity: controller.actionIdentity },
      ],
    };
    const manifestBytes = canonical(manifest);
    const bundleDigest = hash(manifestBytes);
    const installBytes = canonical({ bundleDigest, manifest });
    const archiveBytes = encodeBundleArchive({
      "action.callable.js": await readOwnedStagingFile(actionPath),
      "bundle-install.json": Buffer.from(installBytes, "utf8"),
      "descriptor.json": await readOwnedStagingFile(descriptorPath),
      "integrity-manifest.json": Buffer.from(manifestBytes, "utf8"),
      "render.callable.js": await readOwnedStagingFile(renderPath),
    });
    return { bundleDigest, manifest, archiveBytes: Buffer.from(archiveBytes) };
  } finally {
    if (staging) await rm(staging, { recursive: true, force: true });
  }
}

async function validateAuthoredSources(rootDir: string): Promise<ControllerCompilation> {
  const realRoot = await canonicalPath(rootDir);
  const lexicalRoot = resolve(rootDir);
  const entry = join(rootDir, "controller.tsx");
  await assertNoSymlinkBelow(lexicalRoot, resolve(entry));
  const resolved = await canonicalPath(entry);
  if (!isContainedPath(realRoot, resolved) || resolved === realRoot) throw new Error("authored entrypoint escapes rootDir");
  for (const obsolete of ["action.ts", "render.tsx"]) {
    try {
      await access(join(rootDir, obsolete));
      throw new Error(`legacy authored entrypoint is not allowed: ${obsolete}`);
    } catch (error) {
      if (error instanceof Error && error.message.startsWith("legacy authored entrypoint")) throw error;
    }
  }
  return compileControllerSource(await readFile(entry, "utf8"));
}

/**
 * The controller source is deliberately a tiny declarative language. Validate
 * its AST before Oxc lowers JSX so generated phases cannot accidentally gain
 * module initializers, import aliases, or an unbound action closure.
 */
function compileControllerSource(source: string): ControllerCompilation {
  const program = parseAst(source, { lang: "tsx", sourceType: "module" }) as unknown as AstNode;
  if (program.type !== "Program" || !Array.isArray(program.body)) throw new Error("controller.tsx must be a module");
  if (containsImportExpression(program)) throw new Error("dynamic imports are not allowed in controller.tsx");

  let action: { name: string; identity: string } | undefined;
  let controller: AstNode | undefined;
  const imports = new Set<string>();
  for (const statement of program.body as AstNode[]) {
    switch (statement.type) {
      case "ImportDeclaration":
        validateControllerImport(statement, imports);
        break;
      case "TSInterfaceDeclaration":
        break;
      case "VariableDeclaration": {
        if (action !== undefined) throw new Error("controller.tsx requires exactly one defineAction binding");
        action = validateActionBinding(statement, imports);
        break;
      }
      case "ExportDefaultDeclaration":
        if (controller !== undefined) throw new Error("controller.tsx requires exactly one default defineController");
        if (action === undefined) throw new Error("defineAction must appear before the default defineController");
        controller = validateControllerDeclaration(statement, imports, action.name);
        break;
      default:
        throw new Error(`controller.tsx has an unapproved top-level ${statement.type ?? "statement"}`);
    }
  }
  if (action === undefined) throw new Error("controller.tsx requires exactly one direct defineAction binding");
  if (controller === undefined) throw new Error("controller.tsx requires exactly one direct default defineController");
  assertNoActionReassignment(program, action.name);
  return { source, controller, actionName: action.name, actionIdentity: action.identity };
}

function validateControllerImport(statement: AstNode, imports: Set<string>): void {
  const source = nodeString((statement.source as AstNode | undefined)?.value);
  if (source !== "@oxideloop/kubernetes") throw new Error("controller.tsx imports must come from @oxideloop/kubernetes");
  const specifiers = statement.specifiers;
  if (!Array.isArray(specifiers) || specifiers.length === 0) throw new Error("controller.tsx imports must name approved symbols");
  for (const specifier of specifiers as AstNode[]) {
    if (specifier.type !== "ImportSpecifier") throw new Error("controller.tsx does not allow default or namespace imports");
    const imported = identifierName(specifier.imported as AstNode | undefined);
    const local = identifierName(specifier.local as AstNode | undefined);
    if (!imported || imported !== local || !AUTHORED_IMPORTS.has(imported)) {
      throw new Error("controller.tsx import aliases and unknown imports are not allowed");
    }
    if (imports.has(imported)) throw new Error(`controller.tsx imports ${imported} more than once`);
    imports.add(imported);
  }
}

function validateActionBinding(statement: AstNode, imports: ReadonlySet<string>): { name: string; identity: string } {
  if (statement.kind !== "const" || !Array.isArray(statement.declarations) || statement.declarations.length !== 1) {
    throw new Error("defineAction must be one direct const binding");
  }
  const declarator = statement.declarations[0] as AstNode;
  const name = identifierName(declarator.id as AstNode | undefined);
  const init = declarator.init as AstNode | undefined;
  if (!name || !isDirectCall(init, "defineAction") || !imports.has("defineAction")) {
    throw new Error("defineAction must be a direct unaliased top-level binding");
  }
  const args = init.arguments;
  if (!Array.isArray(args) || args.length !== 1 || (args[0] as AstNode).type !== "ObjectExpression") {
    throw new Error("defineAction requires one literal definition object");
  }
  const properties = exactObjectProperties(args[0] as AstNode, ["id", "run", "version"], "defineAction");
  const identity = nodeString((properties.get("id") as AstNode | undefined)?.value);
  const version = nodeString((properties.get("version") as AstNode | undefined)?.value);
  if (!identity || !version) throw new Error("defineAction id and version must be nonempty string literals");
  const run = properties.get("run") as AstNode | undefined;
  validateActionRun(run, imports);
  return { name, identity };
}

function validateActionRun(run: AstNode | undefined, imports: ReadonlySet<string>): void {
  if (!run || (run.type !== "ArrowFunctionExpression" && run.type !== "FunctionExpression") || run.async === true || run.generator === true) {
    throw new Error("defineAction run must be a synchronous non-generator function");
  }
  if (!Array.isArray(run.params) || run.params.length !== 1) throw new Error("defineAction run must accept { source, metadata }");
  const parameterNames = patternNames(run.params[0] as AstNode);
  if (parameterNames.size !== 2 || !parameterNames.has("source") || !parameterNames.has("metadata")) {
    throw new Error("defineAction run must accept exactly { source, metadata }");
  }
  const locals = new Set(parameterNames);
  const body = run.body as AstNode | undefined;
  let result: AstNode | undefined;
  if (body?.type === "BlockStatement") {
    if (!Array.isArray(body.body)) throw new Error("invalid defineAction body");
    for (const statement of body.body as AstNode[]) {
      if (statement.type === "VariableDeclaration" && statement.kind === "const" && Array.isArray(statement.declarations)) {
        for (const declaration of statement.declarations as AstNode[]) {
          const local = identifierName(declaration.id as AstNode | undefined);
          const init = declaration.init as AstNode | undefined;
          if (!local || !init) throw new Error("action locals must be initialized const values");
          assertActionValue(init, locals, imports);
          locals.add(local);
        }
      } else if (statement.type === "ReturnStatement" && result === undefined) {
        result = statement.argument as AstNode | undefined;
      } else {
        throw new Error("defineAction run may only derive const values and return Event");
      }
    }
  } else {
    result = body;
  }
  if (!result || result.type !== "JSXElement" || jsxName((result.openingElement as AstNode | undefined)?.name as AstNode | undefined) !== "Event") {
    throw new Error("defineAction run must return one Event JSX element");
  }
  assertActionValue(result, locals, imports);
}

function validateControllerDeclaration(statement: AstNode, imports: ReadonlySet<string>, actionName: string): AstNode {
  const declaration = statement.declaration as AstNode | undefined;
  if (!isDirectCall(declaration, "defineController") || !imports.has("defineController")) {
    throw new Error("default export must be one direct unaliased defineController call");
  }
  const args = declaration.arguments;
  if (!Array.isArray(args) || args.length !== 1) throw new Error("defineController requires exactly one render function");
  const render = args[0] as AstNode;
  if ((render.type !== "ArrowFunctionExpression" && render.type !== "FunctionExpression") || render.async === true || render.generator === true) {
    throw new Error("defineController render must be synchronous");
  }
  validateControllerRender(render, imports, actionName);
  return statement;
}

/**
 * The render callback is a capability-limited derivation, not a JavaScript
 * effect phase. Keep its statement grammar intentionally small so calls such
 * as fetch(), timers, constructors, assignments, and delayed closures fail at
 * compile time rather than becoming renderer behaviour.
 */
function validateControllerRender(render: AstNode, imports: ReadonlySet<string>, actionName: string): void {
  if (!Array.isArray(render.params) || render.params.length !== 1 || !hasExactRootParameter(render.params[0] as AstNode)) {
    throw new Error("defineController render must accept exactly ({ root })");
  }
  const values = new Set(["root", actionName]);
  const resourceHandles = new Set<string>();
  const body = render.body as AstNode | undefined;
  if (body?.type !== "BlockStatement") {
    if (!body) throw new Error("defineController render requires a result");
    assertRenderValue(body, values, resourceHandles, imports);
    return;
  }
  if (!Array.isArray(body.body)) throw new Error("invalid defineController body");
  let returned = false;
  for (const statement of body.body as AstNode[]) {
    if (statement.type === "VariableDeclaration" && statement.kind === "const" && Array.isArray(statement.declarations) && !returned) {
      for (const declaration of statement.declarations as AstNode[]) {
        const name = identifierName(declaration.id as AstNode | undefined);
        const init = declaration.init as AstNode | undefined;
        if (!name || !init || values.has(name)) throw new Error("controller locals must be initialized unique const values");
        assertRenderValue(init, values, resourceHandles, imports);
        if (isDirectCapabilityCall(init, "useResource")) resourceHandles.add(name);
        values.add(name);
      }
      continue;
    }
    if (statement.type === "ExpressionStatement" && !returned) {
      const expression = statement.expression as AstNode | undefined;
      if (!isDirectCapabilityCall(expression, "useAutomation")) {
        throw new Error("controller render expressions may only call useAutomation()");
      }
      if (!expression) throw new Error("useAutomation() expression is missing");
      assertRenderValue(expression, values, resourceHandles, imports);
      continue;
    }
    if (statement.type === "ReturnStatement" && !returned) {
      const result = statement.argument as AstNode | undefined;
      if (!result) throw new Error("defineController render must return a semantic tree");
      assertRenderValue(result, values, resourceHandles, imports);
      returned = true;
      continue;
    }
    throw new Error("defineController render may only derive const values, call useAutomation(), and return once");
  }
  if (!returned) throw new Error("defineController render must return a semantic tree");
}

function hasExactRootParameter(parameter: AstNode): boolean {
  if (parameter.type !== "ObjectPattern" || !Array.isArray(parameter.properties) || parameter.properties.length !== 1) return false;
  const property = parameter.properties[0] as AstNode;
  return property.type === "Property" && property.kind === "init" && property.computed !== true &&
    identifierName(property.key as AstNode | undefined) === "root" && identifierName(property.value as AstNode | undefined) === "root";
}

function isDirectCapabilityCall(node: AstNode | undefined, capability: string): boolean {
  return isDirectCall(node, capability) && RENDER_CAPABILITIES.has(capability);
}

function assertRenderValue(node: AstNode, values: ReadonlySet<string>, resourceHandles: ReadonlySet<string>, imports: ReadonlySet<string>): void {
  switch (node.type) {
    case "Literal":
      return;
    case "Identifier": {
      const name = identifierName(node);
      if (!name || (!values.has(name) && !imports.has(name))) throw new Error(`controller render reads unsupported value: ${name ?? "identifier"}`);
      return;
    }
    case "JSXElement":
      assertRenderJsx(node, values, resourceHandles, imports);
      return;
    case "JSXFragment":
      for (const child of node.children as AstNode[] ?? []) {
        if (child.type === "JSXExpressionContainer") assertRenderValue(child.expression as AstNode, values, resourceHandles, imports);
        else if (child.type === "JSXElement") assertRenderJsx(child, values, resourceHandles, imports);
      }
      return;
    case "MemberExpression":
    case "OptionalMemberExpression":
      assertRenderValue(node.object as AstNode, values, resourceHandles, imports);
      if (node.computed === true) assertRenderValue(node.property as AstNode, values, resourceHandles, imports);
      return;
    case "ChainExpression":
      assertRenderValue(node.expression as AstNode, values, resourceHandles, imports);
      return;
    case "ObjectExpression":
      for (const property of node.properties as AstNode[] ?? []) {
        if (property.type !== "Property" || property.kind !== "init" || property.method === true) throw new Error("controller render has an unsupported object value");
        if (property.computed === true) assertRenderValue(property.key as AstNode, values, resourceHandles, imports);
        assertRenderValue(property.value as AstNode, values, resourceHandles, imports);
      }
      return;
    case "ArrayExpression":
      for (const item of node.elements as Array<AstNode | null> ?? []) if (item) assertRenderValue(item, values, resourceHandles, imports);
      return;
    case "TemplateLiteral":
      for (const expression of node.expressions as AstNode[] ?? []) assertRenderValue(expression, values, resourceHandles, imports);
      return;
    case "ConditionalExpression":
      assertRenderValue(node.test as AstNode, values, resourceHandles, imports);
      assertRenderValue(node.consequent as AstNode, values, resourceHandles, imports);
      assertRenderValue(node.alternate as AstNode, values, resourceHandles, imports);
      return;
    case "LogicalExpression":
    case "BinaryExpression":
      assertRenderValue(node.left as AstNode, values, resourceHandles, imports);
      assertRenderValue(node.right as AstNode, values, resourceHandles, imports);
      return;
    case "UnaryExpression":
      assertRenderValue(node.argument as AstNode, values, resourceHandles, imports);
      return;
    case "CallExpression": {
      const capability = identifierName(node.callee as AstNode | undefined);
      if (capability && RENDER_CAPABILITIES.has(capability) && imports.has(capability)) {
        for (const argument of node.arguments as AstNode[] ?? []) assertRenderValue(argument, values, resourceHandles, imports);
        return;
      }
      const callee = node.callee as AstNode | undefined;
      if ((callee?.type === "MemberExpression" || callee?.type === "OptionalMemberExpression") &&
          callee.computed !== true && identifierName(callee.property as AstNode | undefined) === "changed" &&
          resourceHandles.has(identifierName(callee.object as AstNode | undefined) ?? "")) {
        if ((node.arguments as AstNode[] | undefined)?.length !== 0) throw new Error("changed() does not accept arguments");
        return;
      }
      throw new Error("controller render cannot call non-capability code");
    }
    case "AssignmentExpression":
    case "UpdateExpression":
    case "NewExpression":
    case "AwaitExpression":
    case "YieldExpression":
    case "ArrowFunctionExpression":
    case "FunctionExpression":
    case "ThisExpression":
      throw new Error(`controller render contains forbidden ${node.type}`);
    default:
      throw new Error(`controller render contains unsupported ${node.type ?? "syntax"}`);
  }
}

function assertRenderJsx(element: AstNode, values: ReadonlySet<string>, resourceHandles: ReadonlySet<string>, imports: ReadonlySet<string>): void {
  const opening = element.openingElement as AstNode | undefined;
  const name = jsxName(opening?.name as AstNode | undefined);
  if (!name || (!imports.has(name) && !values.has(name))) throw new Error("controller render may use only imported components or useEnrichment wrappers");
  for (const attribute of opening?.attributes as AstNode[] ?? []) {
    if (attribute.type === "JSXSpreadAttribute") throw new Error("controller render does not allow JSX spread props");
    const value = attribute.value as AstNode | undefined;
    if (value?.type === "JSXExpressionContainer") assertRenderValue(value.expression as AstNode, values, resourceHandles, imports);
  }
  for (const child of element.children as AstNode[] ?? []) {
    if (child.type === "JSXExpressionContainer") assertRenderValue(child.expression as AstNode, values, resourceHandles, imports);
    else if (child.type === "JSXElement") assertRenderJsx(child, values, resourceHandles, imports);
    else if (child.type !== "JSXText") throw new Error("controller render has an unsupported JSX child");
  }
}

function assertNoActionReassignment(node: unknown, actionName: string): void {
  walkAst(node, (current) => {
    if (current.type === "AssignmentExpression" || current.type === "UpdateExpression") {
      const target = current.type === "AssignmentExpression" ? current.left as AstNode | undefined : current.argument as AstNode | undefined;
      if (identifierName(target) === actionName) throw new Error("defineAction bindings cannot be reassigned");
    }
  });
}

function exactObjectProperties(object: AstNode, expected: readonly string[], label: string): Map<string, AstNode> {
  const output = new Map<string, AstNode>();
  if (!Array.isArray(object.properties)) throw new Error(`${label} definition must be an object`);
  for (const property of object.properties as AstNode[]) {
    if (property.type !== "Property" || property.computed === true || property.method === true || property.kind !== "init") {
      throw new Error(`${label} definition has an invalid property`);
    }
    const key = identifierName(property.key as AstNode | undefined);
    const value = property.value as AstNode | undefined;
    if (!key || !value || !expected.includes(key) || output.has(key)) throw new Error(`${label} definition has unknown or duplicate fields`);
    output.set(key, value);
  }
  if (output.size !== expected.length || expected.some((key) => !output.has(key))) throw new Error(`${label} definition has missing fields`);
  return output;
}

/** Accept only local, parameter-derived, literal, and imported-component action values. */
function assertActionValue(node: AstNode, locals: ReadonlySet<string>, imports: ReadonlySet<string>): void {
  switch (node.type) {
    case "Literal":
    case "ThisExpression":
      if (node.type === "ThisExpression") throw new Error("defineAction run cannot read this");
      return;
    case "Identifier": {
      const name = identifierName(node);
      if (!name || (!locals.has(name) && !ACTION_COMPONENT_IMPORTS.has(name))) throw new Error(`defineAction run captures unsupported value: ${name ?? "identifier"}`);
      return;
    }
    case "JSXElement":
      assertActionJsx(node, locals, imports);
      return;
    case "JSXFragment":
      throw new Error("defineAction run must return one Event JSX element");
    case "MemberExpression":
    case "OptionalMemberExpression":
      assertActionValue(node.object as AstNode, locals, imports);
      if (node.computed === true) assertActionValue(node.property as AstNode, locals, imports);
      return;
    case "ChainExpression":
      assertActionValue(node.expression as AstNode, locals, imports);
      return;
    case "ObjectExpression":
      for (const property of node.properties as AstNode[] ?? []) {
        if (property.type !== "Property" || property.kind !== "init" || property.method === true) throw new Error("defineAction run has an unsupported object value");
        if (property.computed === true) assertActionValue(property.key as AstNode, locals, imports);
        assertActionValue(property.value as AstNode, locals, imports);
      }
      return;
    case "ArrayExpression":
      for (const item of node.elements as Array<AstNode | null> ?? []) if (item) assertActionValue(item, locals, imports);
      return;
    case "TemplateLiteral":
      for (const expression of node.expressions as AstNode[] ?? []) assertActionValue(expression, locals, imports);
      return;
    case "ConditionalExpression":
    case "LogicalExpression":
    case "BinaryExpression":
      assertActionValue(node.left as AstNode ?? node.test as AstNode, locals, imports);
      assertActionValue(node.right as AstNode ?? node.consequent as AstNode, locals, imports);
      if (node.alternate) assertActionValue(node.alternate as AstNode, locals, imports);
      return;
    case "UnaryExpression":
      assertActionValue(node.argument as AstNode, locals, imports);
      return;
    default:
      throw new Error(`defineAction run contains unsupported ${node.type ?? "syntax"}`);
  }
}

function assertActionJsx(element: AstNode, locals: ReadonlySet<string>, imports: ReadonlySet<string>): void {
  const opening = element.openingElement as AstNode | undefined;
  const name = jsxName(opening?.name as AstNode | undefined);
  if (!name || !ACTION_COMPONENT_IMPORTS.has(name) || !imports.has(name)) throw new Error("defineAction run may use only approved imported JSX components");
  for (const attribute of opening?.attributes as AstNode[] ?? []) {
    if (attribute.type === "JSXSpreadAttribute") throw new Error("defineAction run does not allow JSX spread props");
    const value = attribute.value as AstNode | undefined;
    if (value?.type === "JSXExpressionContainer") assertActionValue(value.expression as AstNode, locals, imports);
  }
  for (const child of element.children as AstNode[] ?? []) {
    if (child.type === "JSXExpressionContainer") assertActionValue(child.expression as AstNode, locals, imports);
    else if (child.type === "JSXElement") assertActionJsx(child, locals, imports);
    else if (child.type !== "JSXText") throw new Error("defineAction run has an unsupported JSX child");
  }
}

function actionModuleSource(controller: ControllerCompilation): string {
  if (controller.controller.start === undefined || controller.controller.end === undefined) {
    throw new Error("controller AST has no source range");
  }
  // The action phase must not retain or evaluate the render definition. It is
  // removed from the already-validated source before the action export is
  // attached, rather than relying on tree-shaking to preserve the phase split.
  return `${controller.source.slice(0, controller.controller.start)}${controller.source.slice(controller.controller.end)}\nimport { renderAction as __oxideloopRenderAction } from "@oxideloop/kubernetes";\nexport function action(input) { return __oxideloopRenderAction(${controller.actionName}, input); }\n`;
}

function renderModuleSource(controller: ControllerCompilation, actionDigest: string): string {
  const declaration = controller.controller.declaration as AstNode | undefined;
  if (controller.controller.start === undefined || declaration?.start === undefined) throw new Error("controller AST has no source range");
  const prefix = controller.source.slice(0, controller.controller.start);
  const suffix = controller.source.slice(declaration.start);
  const actionIdentity = JSON.stringify(controller.actionIdentity);
  return [
    'import { createCompilerActionBindings } from "@oxideloop/sdk";',
    'import { renderController } from "@oxideloop/kubernetes";',
    prefix,
    `const __oxideloopController = ${suffix}`,
    "",
    "export function render(input) {",
    "  const bindings = createCompilerActionBindings([{",
    `    action: ${controller.actionName},`,
    "    bundleDigest: input.bundleDigest,",
    `    actionModuleIdentity: ${actionIdentity},`,
    "    actionModuleDigest: __OXIDELOOP_ACTION_MODULE_DIGEST__,",
    "  }]);",
    "  return renderController(__oxideloopController, input, bindings);",
    "}",
    "",
  ].join("\n");
}

function isDirectCall(node: AstNode | undefined, name: string): node is AstNode {
  return node?.type === "CallExpression" && identifierName(node.callee as AstNode | undefined) === name;
}

function identifierName(node: AstNode | undefined): string | undefined {
  return node?.type === "Identifier" && typeof node.name === "string" ? node.name : undefined;
}

function jsxName(node: AstNode | undefined): string | undefined {
  return node?.type === "JSXIdentifier" && typeof node.name === "string" ? node.name : undefined;
}

function nodeString(value: unknown): string | undefined { return typeof value === "string" ? value : undefined; }

function patternNames(pattern: AstNode): Set<string> {
  const output = new Set<string>();
  walkAst(pattern, (node) => { if (node.type === "Identifier" && typeof node.name === "string") output.add(node.name); });
  return output;
}

function walkAst(node: unknown, visit: (node: AstNode) => void, seen = new WeakSet<object>()): void {
  if (node === null || typeof node !== "object") return;
  if (seen.has(node)) return;
  seen.add(node);
  const ast = node as AstNode;
  visit(ast);
  for (const value of Object.values(ast)) {
    if (value !== node) walkAst(value, visit, seen);
  }
}

async function bundle(
  input: string,
  source: string,
  output: string,
  name: string,
  exportName: "render" | "action",
  workspaceRoot: string,
  authoredRoot: string,
  define: Record<string, string> = {},
): Promise<void> {
  const transformed = transform(input, source, {
    lang: input.endsWith(".tsx") ? "tsx" : "ts",
    jsx: { runtime: "automatic", importSource: "@oxideloop/react-like-runtime" },
  });
  const workingDir = dirname(output);
  const intermediate = join(workingDir, `${basename(input)}.compiled.mjs`);
  await writeFile(intermediate, transformed.code, "utf8");
  const transformedSources = new Map<string, string>();
  const aliases = {
    "@oxideloop/kubernetes": join(workspaceRoot, "packages/kubernetes/src/index.ts"),
    "@oxideloop/sdk": join(workspaceRoot, "packages/sdk/src/index.ts"),
    "@oxideloop/sdk/hooks": join(workspaceRoot, "packages/sdk/src/hooks.ts"),
    "@oxideloop/react-like-runtime": join(workspaceRoot, "packages/react-like-runtime/src/index.ts"),
    "@oxideloop/react-like-runtime/jsx-runtime": join(workspaceRoot, "packages/react-like-runtime/src/jsx-runtime.ts"),
  } as const;
  const canonicalAliases = await canonicalizeAliases(aliases);
  const runtimeRoots = await Promise.all([
    canonicalPath(join(workspaceRoot, "packages/kubernetes/src")),
    canonicalPath(join(workspaceRoot, "packages/sdk/src")),
    canonicalPath(join(workspaceRoot, "packages/react-like-runtime/src")),
  ]);
  const build = await rolldown({
    input: intermediate,
    define: { "process.env.NODE_ENV": JSON.stringify("production"), ...define },
    plugins: [{
      name: "oxideloop-workspace-aliases",
      async resolveId(source: string, importer?: string) {
        // Rolldown may hand plugins virtual ids. Always establish the importer
        // identity before considering an alias, so a foreign module can never
        // borrow the authored graph's trust.
        if (!importer) {
          // The sole importer-less resolution is Rolldown's declared staged
          // entrypoint. Every other graph edge must carry a classified parent.
          if (await sameCanonicalPath(source, intermediate)) return intermediate;
          throw new Error(`unapproved import without importer: ${source}`);
        }
        const state = await classifyImporter(importer, authoredRoot, intermediate, runtimeRoots);
        const alias = canonicalAliases[source as keyof typeof canonicalAliases];
        if (alias) {
          if (state === "unknown") throw new Error(`unapproved alias import from unknown importer: ${source}`);
          return alias;
        }
        if (source.startsWith("/") || isAbsolute(source)) {
          throw new Error(`absolute imports are not allowed: ${source}`);
        }
        if (!source.startsWith(".")) {
          throw new Error(`unapproved bare import from ${state} module: ${source}`);
        }
        if (state === "unknown") throw new Error(`unapproved relative import from unknown importer: ${source}`);
        if (state === "entrypoint") return resolveAuthoredModule(authoredRoot, dirname(input), source);
        if (state === "authored") return resolveAuthoredModule(authoredRoot, dirname(await canonicalPath(importer)), source);
        return resolveTrustedRuntimeModule(runtimeRoots, dirname(await canonicalPath(importer)), source);
      },
      async load(id: string) {
        const root = await canonicalPath(authoredRoot);
        let canonical: string;
        try { canonical = await canonicalPath(id); } catch { return null; }
        const runtime = runtimeRoots.some((runtimeRoot) => isContainedPath(runtimeRoot, canonical));
        if ((!isContainedPath(root, canonical) && !runtime) || !/\.(?:ts|tsx)$/.test(canonical)) return null;
        const cached = transformedSources.get(canonical);
        if (cached !== undefined) return cached;
        const source = await readFile(canonical, "utf8");
        const compiled = transform(canonical, source, {
          lang: canonical.endsWith(".tsx") ? "tsx" : "ts",
          jsx: { runtime: "automatic", importSource: "@oxideloop/react-like-runtime" },
        }).code;
        transformedSources.set(canonical, compiled);
        return compiled;
      },
      transform(code: string) {
        // Rolldown's resolveDynamicImport hook is not invoked for every
        // expression-valued import in this pinned release. Parse each module
        // with Rolldown's pinned Oxc parser so literal and nonliteral AST
        // ImportExpression nodes fail before code generation. This deliberately
        // does not confuse strings, comments, templates, or regular expressions
        // containing the text `import(` with executable syntax.
        if (containsImportExpression(parseAst(code, { lang: "js", sourceType: "module" }))) {
          throw new Error("dynamic imports are not allowed in bundle modules");
        }
        return null;
      },
      async resolveDynamicImport(_specifier: string, importer?: string) {
        // A dynamic import is never part of this static, single-file ABI.
        // This applies to literal and expression specifiers alike.
        if (importer) throw new Error("dynamic imports are not allowed in bundle modules");
        throw new Error("dynamic imports are not allowed without an importer");
      },
    }],
  });
  try {
    const generated = await build.generate({ format: "iife", name, minify: true });
    const chunks = generated.output.filter((item) => item.type === "chunk");
    if (chunks.length !== 1) throw new Error(`expected one chunk for ${input}`);
    const code = (chunks[0] as { code: string }).code;
    const adapter = exportName === "render"
      ? `return ${name}.render(input);`
      : `return ${name}.action(input);`;
    const bridge = "";
    await writeFile(output, `((input)=>{\n${code}\n${bridge}\n${adapter}\n})\n`, "utf8");
  } finally {
    await build.close();
    await rm(intermediate, { force: true });
  }
}

function containsImportExpression(node: unknown, seen = new WeakSet<object>()): boolean {
  if (node === null || typeof node !== "object") return false;
  if (seen.has(node)) return false;
  seen.add(node);
  if ((node as { type?: unknown }).type === "ImportExpression") return true;
  if (Array.isArray(node)) return node.some((value) => containsImportExpression(value, seen));
  return Object.values(node).some((value) => containsImportExpression(value, seen));
}

async function canonicalizeAliases<T extends Record<string, string>>(aliases: T): Promise<{ readonly [K in keyof T]: string }> {
  const entries = await Promise.all(Object.entries(aliases).map(async ([name, path]) => [name, await canonicalPath(path)] as const));
  return Object.fromEntries(entries) as { readonly [K in keyof T]: string };
}

async function resolveAuthoredModule(root: string, from: string, source: string): Promise<string> {
  return resolveStaticTypeScriptModule(root, from, source, "authored");
}

async function resolveTrustedRuntimeModule(roots: readonly string[], from: string, source: string): Promise<string> {
  const owner = roots.find((root) => isContainedPath(root, from));
  if (!owner) throw new Error(`trusted runtime importer escapes fixed roots: ${from}`);
  return resolveStaticTypeScriptModule(owner, from, source, "trusted runtime");
}

async function resolveStaticTypeScriptModule(root: string, from: string, source: string, label: string): Promise<string> {
  const realRoot = await canonicalPath(root);
  const lexicalRoot = resolve(root);
  if (source.startsWith("/") || !source.startsWith(".")) throw new Error(`unapproved authored import: ${source}`);
  if (/\.(?:mjs|cjs|jsx|json|css|tsbuildinfo)$/.test(source)) {
    throw new Error(`unsupported authored import: ${source}`);
  }
  const raw = resolve(from, source);
  const candidates = /\.js$/.test(raw)
    ? [raw.slice(0, -3) + ".ts", raw.slice(0, -3) + ".tsx"]
    : /\.[^/]+$/.test(raw)
      ? [raw]
      : [raw + ".ts", raw + ".tsx", join(raw, "index.ts"), join(raw, "index.tsx")];
  for (const candidate of candidates) {
    try {
      await assertNoSymlinkBelow(lexicalRoot, candidate);
      const resolved = await canonicalPath(candidate);
      if (!isContainedPath(realRoot, resolved) || !/\.(?:ts|tsx)$/.test(resolved)) throw new Error(`${label} import escapes fixed root: ${source}`);
      return resolved;
    } catch (error) {
      if (error instanceof Error && (error.message.includes("escapes rootDir") || error.message.includes("escapes fixed root"))) throw error;
    }
  }
  throw new Error(`cannot resolve authored import ${source}`);
}

async function assertNoSymlinkBelow(root: string, target: string): Promise<void> {
  const relativePath = relative(root, resolve(target));
  if (!isContainedPath(root, resolve(target)) || relativePath === "") throw new Error("authored import escapes rootDir");
  let cursor = root;
  for (const component of relativePath.split(/[/\\]/)) {
    cursor = join(cursor, component);
    try { if ((await lstat(cursor)).isSymbolicLink()) throw new Error("authored import escapes rootDir"); }
    catch (error) { if (error instanceof Error && error.message.includes("escapes rootDir")) throw error; return; }
  }
}

async function canonicalPath(path: string): Promise<string> {
  return realpath(resolve(path));
}

async function sameCanonicalPath(left: string, right: string): Promise<boolean> {
  try { return (await canonicalPath(left)) === (await canonicalPath(right)); }
  catch { return false; }
}

function isContainedPath(root: string, target: string): boolean {
  const rootResolved = resolve(root);
  const targetResolved = resolve(target);
  if (isAbsolute(relative(rootResolved, targetResolved)) || isAbsolute(relative(targetResolved, rootResolved))) return false;
  const rel = relative(rootResolved, targetResolved);
  return rel === "" || (rel !== ".." && !rel.startsWith(`..${sep}`) && !rel.startsWith("../") && !rel.startsWith("..\\"));
}

type ImporterState = "entrypoint" | "authored" | "runtime" | "unknown";

async function classifyImporter(importer: string, root: string, intermediate: string, runtimeRoots: readonly string[]): Promise<ImporterState> {
  let canonical: string;
  try { canonical = await canonicalPath(importer); } catch { return "unknown"; }
  const realRoot = await canonicalPath(root);
  const realIntermediate = await canonicalPath(intermediate);
  if (canonical === realIntermediate) return "entrypoint";
  if (isContainedPath(realRoot, canonical) && /\.(?:ts|tsx)$/.test(canonical)) return "authored";
  if (runtimeRoots.some((runtimeRoot) => isContainedPath(runtimeRoot, canonical)) && /\.(?:ts|tsx)$/.test(canonical)) return "runtime";
  return "unknown";
}

async function readOwnedStagingFile(path: string): Promise<Buffer> {
  const handle = await open(path, "r");
  try {
    const before = await handle.stat();
    if (!before.isFile() || before.size > 128 * 1024) throw new Error(`generated archive source is invalid: ${path}`);
    const bytes = await handle.readFile();
    const after = await handle.stat();
    if (!after.isFile() || before.size !== after.size || bytes.length !== before.size) {
      throw new Error(`generated archive source changed while reading: ${path}`);
    }
    return bytes;
  } finally {
    await handle.close();
  }
}

async function findWorkspaceRoot(from: string): Promise<string> {
  let candidate = from;
  while (dirname(candidate) !== candidate) {
    try {
      await access(join(candidate, "pnpm-workspace.yaml"));
      return candidate;
    } catch { candidate = dirname(candidate); }
  }
  throw new Error(`could not find pnpm workspace root above ${from}`);
}

async function integrity(path: string, root: string): Promise<FileIntegrity> {
  const bytes = await readFile(path);
  return { path: relative(root, path).replaceAll("\\", "/"), length: bytes.byteLength, sha256: hash(bytes) };
}
function hash(value: string | Uint8Array): string { return createHash("sha256").update(value).digest("hex"); }
/** Mirrors the renderer ABI's canonical JSON object ordering (UTF-16 units). */
function canonical(value: unknown): string {
  if (value === null || typeof value === "boolean" || typeof value === "number" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (typeof value !== "object") throw new Error("bundle metadata must be JSON data");
  const object = value as Record<string, unknown>;
  return `{${Object.keys(object).sort(compareUtf16).map((key) => `${JSON.stringify(key)}:${canonical(object[key])}`).join(",")}}`;
}

function compareUtf16(left: string, right: string): number {
  for (let index = 0; index < Math.min(left.length, right.length); index += 1) {
    const difference = left.charCodeAt(index) - right.charCodeAt(index);
    if (difference !== 0) return difference;
  }
  return left.length - right.length;
}
