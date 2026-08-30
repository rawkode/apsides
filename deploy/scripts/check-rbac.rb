#!/usr/bin/env ruby
# frozen_string_literal: true

require "json"
require "yaml"

path = ARGV.fetch(0) { abort "usage: check-rbac.rb KUSTOMIZE_OUTPUT" }
documents = YAML.load_stream(File.read(path))
roles = documents.select { |document| document.is_a?(Hash) && document["kind"] == "Role" }
bindings = documents.select { |document| document.is_a?(Hash) && document["kind"] == "RoleBinding" }
rbac_kinds = documents.map do |document|
  document["kind"] if document.is_a?(Hash) && document["apiVersion"].to_s.start_with?("rbac.authorization.k8s.io/")
end.compact
invariant_errors = []
unexpected_kinds = rbac_kinds - %w[Role RoleBinding]
invariant_errors << "unexpected RBAC kind: #{unexpected_kinds.uniq.join(", ")}" unless unexpected_kinds.empty?
invariant_errors << "expected exactly one namespaced Role" unless roles.length == 1
invariant_errors << "expected exactly one namespaced RoleBinding" unless bindings.length == 1

if bindings.length == 1 && roles.length == 1
  role_metadata = roles.first.fetch("metadata", {})
  invariant_errors << "Role metadata must be exactly {name: oxideloop-controller}" unless role_metadata.keys == ["name"] && role_metadata["name"] == "oxideloop-controller"
  binding = bindings.first
  binding_metadata = binding.fetch("metadata", {})
  invariant_errors << "RoleBinding metadata must be exactly {name: oxideloop-controller}" unless binding_metadata.keys == ["name"] && binding_metadata["name"] == "oxideloop-controller"
  expected_role_ref = {
    "apiGroup" => "rbac.authorization.k8s.io",
    "kind" => "Role",
    "name" => "oxideloop-controller"
  }
  role_ref = binding.fetch("roleRef", {})
  invariant_errors << "RoleBinding must reference the controller Role" unless binding.dig("roleRef", "apiGroup") == "rbac.authorization.k8s.io" &&
    binding.dig("roleRef", "kind") == "Role" &&
    binding.dig("roleRef", "name") == "oxideloop-controller" &&
    role_ref.keys.sort == expected_role_ref.keys.sort &&
    role_ref == expected_role_ref
  subjects = binding.fetch("subjects", [])
  expected_subject = { "kind" => "ServiceAccount", "name" => "oxideloop-controller" }
  invariant_errors << "RoleBinding must contain exactly one controller ServiceAccount subject" unless subjects.length == 1 &&
    subjects.first.is_a?(Hash) &&
    subjects.first.keys.sort == expected_subject.keys.sort &&
    subjects.first == expected_subject
end
abort invariant_errors.join("; ") unless invariant_errors.empty?

expected = [
  { "apiGroups" => ["demo.oxideloop.dev"], "resources" => ["webapplications"], "verbs" => %w[get list watch patch] },
  { "apiGroups" => ["demo.oxideloop.dev"], "resources" => ["webapplications/status"], "verbs" => ["patch"] },
  { "apiGroups" => ["runtime.oxideloop.dev"], "resources" => ["oxideloopstates"], "verbs" => %w[get list watch create patch] },
  { "apiGroups" => ["runtime.oxideloop.dev"], "resources" => ["automationruns"], "verbs" => %w[get list watch create delete] },
  { "apiGroups" => ["runtime.oxideloop.dev"], "resources" => ["automationruns/status"], "verbs" => ["patch"] },
  { "apiGroups" => [""], "resources" => ["configmaps"], "verbs" => %w[get list watch create patch delete] },
  { "apiGroups" => [""], "resources" => ["services"], "verbs" => %w[get list watch create patch delete] },
  { "apiGroups" => ["events.k8s.io"], "resources" => ["events"], "verbs" => %w[get create] }
]

normalize = lambda do |rule|
  abort "RBAC rule must contain only apiGroups, resources, and verbs" unless rule.keys.sort == %w[apiGroups resources verbs]

  {
    "apiGroups" => Array(rule.fetch("apiGroups")).sort,
    "resources" => Array(rule.fetch("resources")).sort,
    "verbs" => Array(rule.fetch("verbs")).sort
  }
end

actual = roles.first.fetch("rules").map { |rule| normalize.call(rule) }.sort_by { |rule| JSON.generate(rule) }
wanted = expected.map { |rule| normalize.call(rule) }.sort_by { |rule| JSON.generate(rule) }
abort "Role rules do not exactly match required_rbac_v0()" unless actual == wanted

puts "RBAC inventory: exact required_rbac_v0()"
