#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
KUBECTL=${KUBECTL:-kubectl}
CHECK_RBAC="$ROOT_DIR/deploy/scripts/check-rbac.rb"

fail() {
  echo "verify-production-no-failpoint: $*" >&2
  exit 1
}

command -v "$KUBECTL" >/dev/null 2>&1 || fail "kubectl is required for Kustomize validation"
command -v ruby >/dev/null 2>&1 || fail "ruby is required for manifest validation"
[ -x "$CHECK_RBAC" ] || fail "RBAC checker is not executable: $CHECK_RBAC"
[ -x "$ROOT_DIR/deploy/scripts/check-rbac-adversarial.sh" ] || fail "RBAC adversarial checker is not executable"

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/oxideloop-verify.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT HUP INT TERM

"$KUBECTL" kustomize "$ROOT_DIR/deploy/controller" >"$WORK_DIR/production.yaml"
"$KUBECTL" kustomize "$ROOT_DIR/deploy/e2e" >"$WORK_DIR/e2e.yaml"
"$CHECK_RBAC" "$WORK_DIR/production.yaml"
"$ROOT_DIR/deploy/scripts/check-rbac-adversarial.sh"

ruby - "$WORK_DIR/production.yaml" "$WORK_DIR/e2e.yaml" <<'RUBY'
require "yaml"

production_path, e2e_path = ARGV
production = YAML.load_stream(File.read(production_path))
e2e = YAML.load_stream(File.read(e2e_path))
deployment = production.find { |d| d.is_a?(Hash) && d["kind"] == "Deployment" }
e2e_deployment = e2e.find { |d| d.is_a?(Hash) && d["kind"] == "Deployment" }
abort "production Deployment missing" unless deployment
abort "e2e Deployment missing" unless e2e_deployment

template = deployment.dig("spec", "template")
pod_spec = template.fetch("spec")
containers = pod_spec.fetch("containers")
abort "production must have exactly controller and renderer containers" unless containers.map { |c| c["name"] }.sort == %w[controller renderer]
controller = containers.find { |c| c["name"] == "controller" }
renderer = containers.find { |c| c["name"] == "renderer" }

abort "production contains failpoint evidence" if production.any? { |document| document.to_s.downcase.include?("failpoint") }
abort "production must not expose TCP ports" if production.any? { |document| document.to_s.match?(/containerPort|hostPort|hostNetwork|tcp/i) }
abort "production must not contain an archive argument" if production.any? { |document| document.to_s.include?("bundle-archive") }
abort "production image must be production-tagged" unless containers.all? { |c| c.fetch("image") == "ghcr.io/oxideloop/oxideloop:production" }
abort "production replicas/strategy changed" unless deployment.dig("spec", "replicas") == 1 && deployment.dig("spec", "strategy", "type") == "Recreate"
abort "pod automountServiceAccountToken must be false" unless pod_spec["automountServiceAccountToken"] == false
sa = production.find { |d| d.is_a?(Hash) && d["kind"] == "ServiceAccount" }
abort "ServiceAccount automountServiceAccountToken must be false" unless sa && sa["automountServiceAccountToken"] == false

volumes = pod_spec.fetch("volumes")
volume_names = volumes.map { |v| v.fetch("name") }
abort "required capability volumes missing" unless %w[bundle-store renderer-uds controller-kube-api].all? { |name| volume_names.include?(name) }
abort "controller-tmp must not be present" if volume_names.include?("controller-tmp") || controller.fetch("volumeMounts", []).any? { |m| m["name"] == "controller-tmp" }
abort "capability volumes must be size-limited emptyDir" unless %w[bundle-store renderer-uds].all? do |name|
  volume = volumes.find { |v| v["name"] == name }
  volume && volume.dig("emptyDir", "sizeLimit")
end
projected = volumes.find { |v| v["name"] == "controller-kube-api" }
sources = projected.dig("projected", "sources") || []
abort "controller-kube-api must be a manual token/CA/namespace projection" unless sources.any? { |s| s.key?("serviceAccountToken") } && sources.any? { |s| s.key?("configMap") } && sources.any? { |s| s.key?("downwardAPI") }

mount = lambda do |container, name|
  container.fetch("volumeMounts").find { |m| m["name"] == name }
end
bundle_controller = mount.call(controller, "bundle-store")
bundle_renderer = mount.call(renderer, "bundle-store")
uds_controller = mount.call(controller, "renderer-uds")
uds_renderer = mount.call(renderer, "renderer-uds")
abort "bundle-store must be controller RW and renderer RO" unless bundle_controller && bundle_controller["readOnly"] == false && bundle_renderer && bundle_renderer["readOnly"] == true
abort "renderer-uds must be renderer RW and controller RO" unless uds_controller && uds_controller["readOnly"] == true && uds_renderer && uds_renderer["readOnly"] == false
token_mount = mount.call(controller, "controller-kube-api")
abort "controller token projection must use the in-cluster default path" unless token_mount && token_mount["readOnly"] == true && token_mount["mountPath"] == "/var/run/secrets/kubernetes.io/serviceaccount"
abort "renderer must not mount controller credentials" if renderer.fetch("volumeMounts").any? { |m| m["name"] == "controller-kube-api" }
abort "renderer must not receive credential env" if renderer.fetch("env", []).any? { |e| e["name"].to_s.match?(/TOKEN|KUBE|CA|NAMESPACE/i) }
abort "renderer command must explicitly select the worker binary" unless renderer["command"] == ["/usr/local/bin/renderer-worker"]

security = lambda do |object|
  context = object.fetch("securityContext")
  abort "security context must be nonroot" unless context["runAsNonRoot"] == true && context["runAsUser"] == 65_532 && context["runAsGroup"] == 65_532 && context["allowPrivilegeEscalation"] == false && context["readOnlyRootFilesystem"] == true && context.dig("seccompProfile", "type") == "RuntimeDefault" && context.dig("capabilities", "drop") == ["ALL"]
end
security.call(controller)
security.call(renderer)
pod_security = pod_spec.fetch("securityContext")
abort "pod security context must pin nonroot UID/GID and RuntimeDefault" unless pod_security["runAsNonRoot"] == true && pod_security["runAsUser"] == 65_532 && pod_security["runAsGroup"] == 65_532 && pod_security.dig("seccompProfile", "type") == "RuntimeDefault"

abort "controller CLI drifted" unless controller["args"] == ["--namespace", "$(POD_NAMESPACE)", "--root-name", "$(ROOT_NAME)", "--bundle-store", "/var/lib/oxideloop/bundles", "--renderer-unix-socket", "/run/oxideloop/renderer/renderer.sock"]
abort "renderer CLI drifted" unless renderer["args"] == ["--listen-unix", "/run/oxideloop/renderer/renderer.sock", "--store-root", "/var/lib/oxideloop/bundles"]

e2e_pod_spec = e2e_deployment.dig("spec", "template", "spec")
e2e_containers = e2e_pod_spec["containers"]
e2e_controller = e2e_containers&.find { |c| c["name"] == "controller" }
e2e_renderer = e2e_containers&.find { |c| c["name"] == "renderer" }
e2e_init = e2e_pod_spec.fetch("initContainers", []).find { |c| c["name"] == "prepare-failpoint-directory" }
abort "e2e must be clearly non-production" unless e2e_deployment.dig("metadata", "annotations", "oxideloop.dev/non-production") == "true" && e2e_deployment.dig("spec", "template", "metadata", "labels", "oxideloop.dev/environment") == "e2e-only"
abort "e2e must use the separate failpoint image only for controller" unless e2e_controller && e2e_controller["image"] == "ghcr.io/oxideloop/oxideloop:e2e-failpoints" && e2e_renderer && e2e_renderer["image"] == "ghcr.io/oxideloop/oxideloop:production"
abort "e2e controller failpoint socket is missing" unless e2e_controller && e2e_controller.fetch("args").include?("--failpoint-unix-socket") && e2e_controller.fetch("volumeMounts").any? { |m| m["name"] == "failpoint-control" && m["readOnly"] == false && m["subPath"] == "control" }
abort "e2e renderer must not receive the failpoint volume" if e2e_renderer.fetch("volumeMounts").any? { |m| m["name"] == "failpoint-control" }
abort "e2e failpoint init container is missing" unless e2e_init
abort "e2e failpoint init must use the image's dedicated BusyBox mechanism" unless e2e_init["image"] == "ghcr.io/oxideloop/oxideloop:e2e-failpoints" && e2e_init["command"] == ["/bin/busybox", "sh", "-ceu"]
abort "e2e failpoint init must create, own, and chmod the child directory" unless e2e_init.fetch("args").any? { |arg| arg.include?("mkdir -p /run/oxideloop/failpoints/control") && arg.include?("chmod 0700 /run/oxideloop/failpoints/control") && arg.include?("stat -c %u /run/oxideloop/failpoints/control") && arg.include?("= 65532") && arg.include?("stat -c %a /run/oxideloop/failpoints/control") && arg.include?("= 700") }
abort "e2e failpoint init must mount the raw volume without subPath" unless e2e_init.fetch("volumeMounts").any? { |m| m["name"] == "failpoint-control" && m["mountPath"] == "/run/oxideloop/failpoints" && m["subPath"].nil? && m["readOnly"] == false }
abort "e2e failpoint init must be hardened nonroot" unless e2e_init.dig("securityContext", "runAsNonRoot") == true && e2e_init.dig("securityContext", "runAsUser") == 65_532 && e2e_init.dig("securityContext", "runAsGroup") == 65_532 && e2e_init.dig("securityContext", "allowPrivilegeEscalation") == false && e2e_init.dig("securityContext", "readOnlyRootFilesystem") == true && e2e_init.dig("securityContext", "capabilities", "drop") == ["ALL"] && e2e_init.dig("securityContext", "seccompProfile", "type") == "RuntimeDefault"

puts "production/e2e manifest isolation: verified"
RUBY

# Keep the Dockerfile's two build paths auditable even when Docker is not
# installed on the workstation.
dockerfile=$ROOT_DIR/Dockerfile
grep -F 'FROM gcr.io/distroless/cc-debian12:nonroot AS production' "$dockerfile" >/dev/null || fail "production runtime target missing"
grep -F 'FROM gcr.io/distroless/cc-debian12:nonroot AS e2e-failpoints' "$dockerfile" >/dev/null || fail "e2e runtime target missing"
grep -F 'cargo build --locked --release -p oxideloop-controller-runtime --bin oxideloop-controller' "$dockerfile" >/dev/null || fail "production controller release build missing"
grep -F 'cargo build --locked -p oxideloop-controller-runtime --features test-failpoints --bin oxideloop-controller' "$dockerfile" >/dev/null || fail "debug failpoint build missing"
grep -F 'cargo build --locked -p oxideloop-controller-runtime --features test-failpoints --bin oxideloop-e2e-failpoint-control' "$dockerfile" >/dev/null || fail "e2e control client build missing"
grep -F 'target/release/oxideloop-controller' "$dockerfile" >/dev/null || fail "production controller artifact path missing"
grep -F 'target/debug/oxideloop-controller' "$dockerfile" >/dev/null || fail "e2e controller artifact path missing"
grep -F 'target/debug/oxideloop-e2e-failpoint-control' "$dockerfile" >/dev/null || fail "e2e control client artifact path missing"
grep -F 'COPY --from=e2e-tools /bin/busybox /bin/busybox' "$dockerfile" >/dev/null || fail "e2e failpoint init utility missing"
last_target=$(awk '/^FROM / { target=$NF } END { print target }' "$dockerfile")
[ "$last_target" = "production" ] || fail "production must be the safe default final Dockerfile target"

check_binary() {
  binary=$1
  command -v strings >/dev/null 2>&1 || fail "strings is required for binary inspection"
  if strings "$binary" 2>/dev/null | grep -qi 'failpoint'; then
    fail "production controller contains failpoint strings: $binary"
  fi
  set +e
  output=$("$binary" --failpoint-unix-socket /tmp/oxideloop-failpoint.sock 2>&1)
  status=$?
  set -e
  [ "$status" -ne 0 ] || fail "production controller accepted the debug failpoint flag: $binary"
  # The rejected argument is necessarily present in the invocation's error
  # text; only a usage/help line containing the debug flag is evidence that the
  # production parser exposes it.
  if echo "$output" | grep -E '^usage:.*failpoint-unix-socket' >/dev/null; then
    fail "production controller usage exposes the debug failpoint flag: $binary"
  fi
  help_output=$("$binary" --help 2>&1 || true)
  if echo "$help_output" | grep -q 'bundle-archive'; then
    fail "production controller still exposes the removed archive input: $binary"
  fi
}

runtime_tool=""
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  runtime_tool=docker
elif command -v podman >/dev/null 2>&1 && podman info >/dev/null 2>&1; then
  runtime_tool=podman
fi

if [ -n "${PRODUCTION_CONTROLLER_BINARY:-}" ]; then
  [ -x "$PRODUCTION_CONTROLLER_BINARY" ] || fail "PRODUCTION_CONTROLLER_BINARY is not executable"
  check_binary "$PRODUCTION_CONTROLLER_BINARY"
  echo "host production binary check: passed (not image proof)"
fi

if [ -n "$runtime_tool" ]; then
  production_image=${PRODUCTION_IMAGE:-oxideloop/oxideloop:production}
  e2e_image=${E2E_IMAGE:-oxideloop/oxideloop:e2e-failpoints}
  echo "runtime image verification: building $production_image (target production)"
  "$runtime_tool" build --target production --tag "$production_image" "$ROOT_DIR"
  echo "runtime image verification: building $e2e_image (target e2e-failpoints)"
  "$runtime_tool" build --target e2e-failpoints --tag "$e2e_image" "$ROOT_DIR"

  inspect_image() {
    image=$1
    expected_entrypoint=$2
    entrypoint=$("$runtime_tool" image inspect --format '{{json .Config.Entrypoint}}' "$image")
    [ "$entrypoint" = "[\"$expected_entrypoint\"]" ] || fail "$image entrypoint is not $expected_entrypoint: $entrypoint"
    user=$("$runtime_tool" image inspect --format '{{.Config.User}}' "$image")
    [ "$user" = "65532:65532" ] || fail "$image user is not fixed nonroot 65532:65532: $user"
  }

  export_image_tree() {
    image=$1
    label=$2
    container=$("$runtime_tool" create "$image")
    "$runtime_tool" export "$container" >"$WORK_DIR/$label.tar"
    "$runtime_tool" cp "$container:/usr/local/bin/oxideloop-controller" "$WORK_DIR/$label-controller"
    "$runtime_tool" cp "$container:/usr/local/bin/renderer-worker" "$WORK_DIR/$label-renderer"
    "$runtime_tool" rm "$container" >/dev/null
    tar -tf "$WORK_DIR/$label.tar" >"$WORK_DIR/$label.files"
    source_tree_entries=$(grep -E '(^|/)(src|target|Cargo\.toml|Cargo\.lock|\.git)(/|$)' "$WORK_DIR/$label.files" | grep -vE '^usr/src/?$' || true)
    [ -z "$source_tree_entries" ] || fail "$image contains source/build-tree content"
    grep -E '(^|/)(usr/bin/(apt|apt-get|dpkg)|var/lib/(apt|dpkg))(/|$)' "$WORK_DIR/$label.files" >/dev/null && fail "$image contains package-manager/cache content"
  }

  inspect_image "$production_image" /usr/local/bin/oxideloop-controller
  inspect_image "$e2e_image" /usr/local/bin/oxideloop-controller
  export_image_tree "$production_image" production-image
  export_image_tree "$e2e_image" e2e-image
  grep -E '(^|/)etc/ssl/certs/ca-certificates\.crt$' "$WORK_DIR/production-image.files" >/dev/null || fail "production image is missing the CA bundle required by rustls"
  check_binary "$WORK_DIR/production-image-controller"
  if grep -F 'usr/local/bin/oxideloop-e2e-failpoint-control' "$WORK_DIR/production-image.files" >/dev/null; then
    fail "production image contains the e2e control client"
  fi
  grep -F 'usr/local/bin/oxideloop-e2e-failpoint-control' "$WORK_DIR/e2e-image.files" >/dev/null || fail "e2e image is missing the e2e control client"
  command -v strings >/dev/null 2>&1 || fail "strings is required for image binary inspection"
  if strings "$WORK_DIR/production-image-renderer" 2>/dev/null | grep -qiE 'evaluator-binary|failpoint'; then
    fail "production renderer contains debug listener/flag evidence"
  fi
  e2e_help=$("$runtime_tool" run --rm --entrypoint /usr/local/bin/oxideloop-controller "$e2e_image" --help 2>&1 || true)
  echo "$e2e_help" | grep -E '^usage:.*failpoint-unix-socket' >/dev/null || fail "e2e image does not expose its debug failpoint surface"
  control_help=$("$runtime_tool" run --rm --entrypoint /usr/local/bin/oxideloop-e2e-failpoint-control "$e2e_image" --help 2>&1 || true)
  echo "$control_help" | grep -F 'AfterStateFinalizerBeforeRootFinalizer' >/dev/null || fail "e2e control client does not expose the deletion barrier"
  echo "runtime image verification: passed for exact production/e2e targets"
else
  echo "runtime image/content verification: SKIPPED (Docker/Podman unavailable; no image proof claimed)" >&2
fi

echo "static manifest/Dockerfile verification: passed"
