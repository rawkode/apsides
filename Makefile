SHELL := /bin/bash
export CI := 1

OXIDELOOP ?= target/debug/oxideloop
EXAMPLE_CLUSTER ?= oxideloop-example
EXAMPLE_CONTEXT ?= kind-$(EXAMPLE_CLUSTER)
EXAMPLE_NAMESPACE ?= oxideloop-example
EXAMPLE_ROOT ?= root
EXAMPLE_CONTROLLER_IMAGE ?= oxideloop-example-controller
EXAMPLE_APP_IMAGE ?= nginxinc/nginx-unprivileged:alpine
EXAMPLE_LOCAL_PORT ?= 8080
EXAMPLE_WORK ?= work/example
EXAMPLE_RECEIPT := $(EXAMPLE_WORK)/receipt.json
KUBECTL := kubectl --context $(EXAMPLE_CONTEXT) --namespace $(EXAMPLE_NAMESPACE)

.PHONY: example example-cli example-build example-deploy example-status example-forward example-clean
.NOTPARALLEL: example

example: example-deploy example-status
	@printf '\nOxideLoop is running. Open http://localhost:$(EXAMPLE_LOCAL_PORT) after running make example-forward in another terminal.\n'

example-cli:
	cargo build --locked -p legacy-cli

example-build: example-cli
	$(OXIDELOOP) build \
		--source packages/example-bundle/src \
		--image $(EXAMPLE_CONTROLLER_IMAGE) \
		--output $(EXAMPLE_WORK)

example-deploy: example-build
	$(OXIDELOOP) deploy \
		--build $(EXAMPLE_RECEIPT) \
		--kind-cluster $(EXAMPLE_CLUSTER) \
		--namespace $(EXAMPLE_NAMESPACE) \
		--root-name $(EXAMPLE_ROOT) \
		--workload-image $(EXAMPLE_APP_IMAGE)

example-status:
	@$(KUBECTL) get webapplication/$(EXAMPLE_ROOT) configmap/$(EXAMPLE_ROOT)-config deployment/$(EXAMPLE_ROOT) service/$(EXAMPLE_ROOT)-service
	@$(KUBECTL) get pods,oxideloopstates
	@$(KUBECTL) get configmap/$(EXAMPLE_ROOT)-config -o jsonpath='rendered message: {.data.message}{"\n"}'
	@$(KUBECTL) get automationruns -o custom-columns='NAME:.metadata.name,STATE:.status.completion.state'
	@$(KUBECTL) get events.events.k8s.io --field-selector reason=ConfigurationChanged -o custom-columns='REASON:.reason,NOTE:.note'

example-forward:
	$(KUBECTL) port-forward service/$(EXAMPLE_ROOT)-service $(EXAMPLE_LOCAL_PORT):80

example-clean:
	kind delete cluster --name $(EXAMPLE_CLUSTER)
