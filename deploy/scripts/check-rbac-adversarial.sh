#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
CHECKER="$ROOT_DIR/deploy/scripts/check-rbac.rb"
KUBECTL=${KUBECTL:-kubectl}
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/oxideloop-rbac.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT HUP INT TERM

"$KUBECTL" kustomize "$ROOT_DIR/deploy/controller" >"$WORK_DIR/production.yaml"
ruby - "$WORK_DIR/production.yaml" "$WORK_DIR/valid.yaml" <<'RUBY'
require "yaml"

source, destination = ARGV
documents = YAML.load_stream(File.read(source)).select { |document| document.is_a?(Hash) }
selected = documents.select { |document| %w[Role RoleBinding].include?(document["kind"]) }
abort "production baseline must contain one Role and one RoleBinding" unless selected.map { |document| document["kind"] }.sort == %w[Role RoleBinding]
File.write(destination, YAML.dump_stream(*selected))
RUBY

"$CHECKER" "$WORK_DIR/valid.yaml" >/dev/null

ruby - "$WORK_DIR/valid.yaml" "$WORK_DIR" <<'RUBY'
require "yaml"

source, output_directory = ARGV
baseline = YAML.load_stream(File.read(source))
clone = lambda { |documents| Marshal.load(Marshal.dump(documents)) }
write = lambda do |name, documents|
  File.write(File.join(output_directory, "#{name}.yaml"), YAML.dump_stream(*documents))
end

role = baseline.find { |document| document["kind"] == "Role" }
binding = baseline.find { |document| document["kind"] == "RoleBinding" }

documents = clone.call(baseline)
extra = clone.call(role)
extra["kind"] = "ClusterRole"
extra["metadata"]["name"] = "oxideloop-cluster-role"
documents << extra
write.call("cluster-role", documents)

documents = clone.call(baseline)
extra = clone.call(binding)
extra["kind"] = "ClusterRoleBinding"
extra["metadata"]["name"] = "oxideloop-cluster-binding"
documents << extra
write.call("cluster-role-binding", documents)

documents = clone.call(baseline)
extra = clone.call(role)
extra["metadata"]["name"] = "oxideloop-controller-other"
documents << extra
write.call("second-role", documents)

documents = clone.call(baseline)
extra = clone.call(binding)
extra["metadata"]["name"] = "oxideloop-controller-other"
documents << extra
write.call("second-role-binding", documents)

documents = clone.call(baseline)
documents.find { |document| document["kind"] == "RoleBinding" }["subjects"] << { "kind" => "Group", "name" => "system:masters" }
write.call("extra-subject", documents)

documents = clone.call(baseline)
documents.find { |document| document["kind"] == "RoleBinding" }["subjects"][0]["name"] = "other-controller"
write.call("changed-subject", documents)

documents = clone.call(baseline)
documents.find { |document| document["kind"] == "RoleBinding" }["roleRef"]["name"] = "other-role"
write.call("changed-role-ref", documents)
RUBY

for mutation in cluster-role cluster-role-binding second-role second-role-binding extra-subject changed-subject changed-role-ref; do
  if "$CHECKER" "$WORK_DIR/$mutation.yaml" >/dev/null 2>&1; then
    echo "check-rbac-adversarial: checker accepted $mutation" >&2
    exit 1
  fi
  echo "check-rbac-adversarial: rejected $mutation"
done
