use crate::{Result, only};
use serde_json::json;
use std::{
    collections::BTreeMap,
    env, fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const PROJECT_FILES: &[(&str, &str)] = &[
    ("program.tsx", PROGRAM),
    ("deno.json", DENO_CONFIG),
    ("tsconfig.json", TYPESCRIPT_CONFIG),
    (".gitignore", ".aps/\n"),
];

const SDK_FILES: &[(&str, &str)] = &[
    (
        "index.ts",
        include_str!("../../../packages/reakt/src/index.ts"),
    ),
    (
        "model.ts",
        include_str!("../../../packages/reakt/src/model.ts"),
    ),
    (
        "runtime.ts",
        include_str!("../../../packages/reakt/src/runtime.ts"),
    ),
    (
        "compiler.ts",
        include_str!("../../../packages/reakt/src/compiler.ts"),
    ),
    (
        "jsx-runtime.ts",
        include_str!("../../../packages/reakt/src/jsx-runtime.ts"),
    ),
    (
        "jsx-dev-runtime.ts",
        include_str!("../../../packages/reakt/src/jsx-dev-runtime.ts"),
    ),
];

const PROGRAM: &str = r#"import {
  ConfigMap,
  Controller,
  Deployment,
  useConfigMap,
  type DomainBudgets,
} from "@reakt/kubernetes";

const budgets: DomainBudgets = {
  maxResources: 8,
  maxInputs: 8,
  maxReactions: 8,
  maxObjectBytes: 262_144,
  maxQueueDepth: 32,
  maxConcurrentRequests: 2,
};

function App() {
  const settings = useConfigMap({ name: "app-settings" });
  const labels = { app: "hello" };

  return (
    <>
      <ConfigMap
        metadata={{ name: "hello-config", namespace: "default" }}
        data={{ LOG_LEVEL: "info" }}
      />
      <Deployment
        metadata={{ name: "hello", namespace: "default" }}
        rolloutOn={[settings.data]}
        spec={{
          replicas: 1,
          selector: { matchLabels: labels },
          template: {
            metadata: { labels },
            spec: {
              automountServiceAccountToken: false,
              containers: [{
                name: "hello",
                image: "docker.io/nginxinc/nginx-unprivileged@sha256:9b87ad3dd9f431c733f19dfb278c7eb3dba9dca381942c79818bb42f1a566a83",
                ports: [{ containerPort: 8080 }],
                resources: {
                  requests: { cpu: "10m", memory: "16Mi" },
                  limits: { memory: "64Mi" },
                },
                envFrom: [{ configMapRef: { name: "app-settings" } }],
              }],
            },
          },
        }}
      />
    </>
  );
}

export default (
  <Controller
    id="hello"
    namespace="default"
    controlNamespace="apsides-system"
    clusterBinding="local"
    policyRef="platform/hello"
    enforcement="api-bound"
    budgets={budgets}
  >
    <App />
  </Controller>
);
"#;

const DENO_CONFIG: &str = r#"{
  "compilerOptions": {
    "jsx": "react-jsxdev",
    "jsxImportSource": "@reakt/kubernetes",
    "strict": true
  },
  "imports": {
    "@reakt/kubernetes": "./.aps/sdk/index.ts",
    "@reakt/kubernetes/jsx-runtime": "./.aps/sdk/jsx-runtime.ts",
    "@reakt/kubernetes/jsx-dev-runtime": "./.aps/sdk/jsx-dev-runtime.ts"
  }
}
"#;

const TYPESCRIPT_CONFIG: &str = r#"{
  "compilerOptions": {
    "allowImportingTsExtensions": true,
    "baseUrl": ".",
    "jsx": "react-jsx",
    "jsxImportSource": "@reakt/kubernetes",
    "module": "ESNext",
    "moduleResolution": "Bundler",
    "noEmit": true,
    "paths": {
      "@reakt/kubernetes": ["./.aps/sdk/index.ts"],
      "@reakt/kubernetes/jsx-runtime": ["./.aps/sdk/jsx-runtime.ts"],
      "@reakt/kubernetes/jsx-dev-runtime": ["./.aps/sdk/jsx-dev-runtime.ts"]
    },
    "strict": true,
    "target": "ES2022"
  },
  "include": ["**/*.ts", "**/*.tsx"],
  "exclude": ["node_modules", ".aps"]
}
"#;

/// Create one modest TypeScript authoring project in an empty file namespace.
///
/// # Errors
/// Rejects unknown options, existing generated paths, unavailable compiler assets,
/// source-lock failures, symlinks and filesystem races without overwriting files.
pub(crate) fn command(options: &BTreeMap<String, String>) -> Result<()> {
    only(options, &["--directory"])?;
    let requested = options.get("--directory").map_or(".", String::as_str);
    let destination = destination(Path::new(requested))?;
    initialize(&destination)?;
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "initialized",
            "directory": destination,
            "entry": "program.tsx",
            "sourceLock": "reakt.lock.json"
        }))?
    )?;
    Ok(())
}

fn destination(requested: &Path) -> Result<PathBuf> {
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        env::current_dir()?.join(requested)
    };
    match fs::symlink_metadata(&absolute) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("init directory must be a real directory".into());
            }
            Ok(absolute.canonicalize()?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = absolute
                .parent()
                .ok_or("init directory must have an existing parent")?
                .canonicalize()?;
            let name = absolute
                .file_name()
                .ok_or("init directory name is invalid")?;
            Ok(parent.join(name))
        }
        Err(error) => Err(error.into()),
    }
}

fn initialize(destination: &Path) -> Result<()> {
    reject_existing_outputs(destination)?;
    let parent = destination
        .parent()
        .ok_or("init directory must have an existing parent")?;
    let stage = create_stage(parent)?;
    let result = (|| {
        write_stage(&stage)?;
        generate_source_lock(&stage)?;
        publish(&stage, destination)
    })();
    let _ = fs::remove_dir_all(&stage);
    result
}

fn reject_existing_outputs(destination: &Path) -> Result<()> {
    for name in PROJECT_FILES
        .iter()
        .map(|(name, _)| *name)
        .chain(["reakt.lock.json", ".aps"])
    {
        if fs::symlink_metadata(destination.join(name)).is_ok() {
            return Err(format!("init refuses to overwrite {name}").into());
        }
    }
    Ok(())
}

fn create_stage(parent: &Path) -> Result<PathBuf> {
    for attempt in 0_u8..100 {
        let path = parent.join(format!(".aps-init-{}-{attempt}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err("unable to allocate an init staging directory".into())
}

fn write_stage(stage: &Path) -> Result<()> {
    for (name, contents) in PROJECT_FILES {
        fs::write(stage.join(name), contents)?;
    }
    let sdk = stage.join(".aps/sdk");
    fs::create_dir_all(&sdk)?;
    for (name, contents) in SDK_FILES {
        fs::write(sdk.join(name), contents)?;
    }
    Ok(())
}

fn generate_source_lock(stage: &Path) -> Result<()> {
    let script = crate::assets::root()?.join("packages/compiler/src/source-lock.ts");
    let metadata =
        fs::symlink_metadata(&script).map_err(|_| "Apsides compiler assets are unavailable")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Apsides compiler assets are invalid".into());
    }
    let status = Command::new("node")
        .arg(script.canonicalize()?)
        .arg(stage.join("program.tsx"))
        .env_clear()
        .env("PATH", env::var_os("PATH").unwrap_or_default())
        .stdout(Stdio::null())
        .status()?;
    if !status.success() {
        return Err("source lock generation failed".into());
    }
    Ok(())
}

fn publish(stage: &Path, destination: &Path) -> Result<()> {
    reject_existing_outputs(destination)?;
    let destination_created = match fs::create_dir(destination) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    let mut created = Vec::new();
    let result = (|| {
        let generated = destination.join(".aps");
        fs::create_dir(&generated)?;
        created.push(generated.clone());
        let sdk = generated.join("sdk");
        fs::create_dir(&sdk)?;
        for (name, _) in SDK_FILES {
            copy_exclusive(&stage.join(".aps/sdk").join(name), &sdk.join(name))?;
        }
        for name in PROJECT_FILES
            .iter()
            .map(|(name, _)| *name)
            .chain(["reakt.lock.json"])
        {
            let target = destination.join(name);
            copy_exclusive(&stage.join(name), &target)?;
            created.push(target);
        }
        Ok(())
    })();
    if let Err(error) = result {
        for path in created.iter().rev() {
            if path.is_dir() {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
        if destination_created {
            let _ = fs::remove_dir(destination);
        }
        return Err(error);
    }
    Ok(())
}

fn copy_exclusive(source: &Path, destination: &Path) -> Result<()> {
    let contents = fs::read(source)?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    if let Err(error) = output.write_all(&contents) {
        drop(output);
        let _ = fs::remove_file(destination);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use sha2::{Digest as _, Sha256};

    #[test]
    fn scaffold_is_locked_and_typechecks_outside_the_repository() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let project = temporary.path().join("hello");
        initialize(&project)?;

        let lock: Value = serde_json::from_slice(&fs::read(project.join("reakt.lock.json"))?)?;
        let program = fs::read(project.join("program.tsx"))?;
        assert_eq!(lock["entry"], "program.tsx");
        assert_eq!(lock["inputs"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            lock["inputs"][0]["sha256"],
            format!("{:x}", Sha256::digest(program))
        );
        assert!(project.join(".aps/sdk/index.ts").is_file());
        assert_eq!(fs::read_to_string(project.join(".gitignore"))?, ".aps/\n");

        let status = Command::new("deno")
            .args([
                "check",
                "--config",
                "deno.json",
                "--no-lock",
                "--no-npm",
                "--no-remote",
                "--deny-import",
                "program.tsx",
            ])
            .current_dir(&project)
            .status()?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn existing_project_file_is_never_overwritten() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let project = temporary.path().join("hello");
        fs::create_dir(&project)?;
        fs::write(project.join("program.tsx"), "keep me")?;

        assert!(initialize(&project).is_err());
        assert_eq!(fs::read_to_string(project.join("program.tsx"))?, "keep me");
        assert!(!project.join(".aps").exists());
        assert!(!project.join("reakt.lock.json").exists());
        Ok(())
    }
}
