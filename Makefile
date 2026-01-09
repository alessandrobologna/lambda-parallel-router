SHELL := bash

.DEFAULT_GOAL := help

AWS_REGION ?= $(shell aws configure get region)
AWS_ACCOUNT_ID ?= $(shell aws sts get-caller-identity --query Account --output text)
ECR_REGISTRY ?= $(AWS_ACCOUNT_ID).dkr.ecr.$(AWS_REGION).amazonaws.com

ROUTER_REPO_PREFIX ?= lambda-parallel-router
ROUTER_REPO_NAME ?= $(ROUTER_REPO_PREFIX)/router
ROUTER_VERSION ?= $(shell tr -d '\n' < VERSION)
ROUTER_IMAGE_TAG ?= $(ROUTER_VERSION)
ROUTER_IMAGE_PLATFORM ?= linux/amd64
ROUTER_IMAGE_IDENTIFIER ?= $(ECR_REGISTRY)/$(ROUTER_REPO_NAME):$(ROUTER_IMAGE_TAG)

SAM_DEPLOY_FLAGS ?= --resolve-s3 --capabilities CAPABILITY_IAM --no-confirm-changeset --no-fail-on-empty-changeset

BOOTSTRAP_STACK_NAME ?= lpr-bootstrap
BOOTSTRAP_TEMPLATE ?= bootstrap/template.yaml
BOOTSTRAP_BUCKET ?=

.PHONY: help
help:
	@printf '%s\n' \
		'Targets:' \
		'  make deploy              Deploy bootstrap + push image + deploy demo stack' \
		'  make bootstrap-deploy     Deploy the bootstrap stack (macro + shared config bucket)' \
		'  make ecr-template         Create/update ECR repository creation template (CREATE_ON_PUSH)' \
		'  make ecr-login            Docker login to ECR' \
		'  make image-build          Build router container image' \
		'  make image-push           Push router container image (auto-creates repo on first push)' \
		'  make sam-build            sam build' \
		'  make sam-deploy           sam deploy (uses sam/samconfig.toml)' \
		'  make print-vars           Show computed variables' \
		'  make ecr-template-delete  Delete the ECR repository creation template' \
		'' \
		'Common overrides:' \
		'  make deploy BOOTSTRAP_BUCKET=my-existing-bucket' \
		'  make deploy ROUTER_REPO_PREFIX=my-prefix ROUTER_REPO_NAME=my-prefix/router ROUTER_IMAGE_TAG=latest'

.PHONY: check
check:
	@if [[ -z "$(AWS_REGION)" ]]; then echo "AWS_REGION is empty (set AWS_REGION or configure a default region)"; exit 1; fi
	@if [[ -z "$(AWS_ACCOUNT_ID)" ]]; then echo "Failed to resolve AWS_ACCOUNT_ID (check AWS credentials)"; exit 1; fi
	@case "$(ROUTER_REPO_NAME)" in \
		"$(ROUTER_REPO_PREFIX)"/*) ;; \
		*) echo "ROUTER_REPO_NAME must start with ROUTER_REPO_PREFIX/ (got $(ROUTER_REPO_NAME), prefix $(ROUTER_REPO_PREFIX))"; exit 1 ;; \
	esac

.PHONY: print-vars
print-vars: check
	@printf '%s\n' \
		"AWS_REGION=$(AWS_REGION)" \
		"AWS_ACCOUNT_ID=$(AWS_ACCOUNT_ID)" \
		"ECR_REGISTRY=$(ECR_REGISTRY)" \
		"ROUTER_REPO_PREFIX=$(ROUTER_REPO_PREFIX)" \
		"ROUTER_REPO_NAME=$(ROUTER_REPO_NAME)" \
		"ROUTER_IMAGE_TAG=$(ROUTER_IMAGE_TAG)" \
		"ROUTER_IMAGE_IDENTIFIER=$(ROUTER_IMAGE_IDENTIFIER)" \
		"BOOTSTRAP_STACK_NAME=$(BOOTSTRAP_STACK_NAME)" \
		"BOOTSTRAP_TEMPLATE=$(BOOTSTRAP_TEMPLATE)" \
		"BOOTSTRAP_BUCKET=$(BOOTSTRAP_BUCKET)"

.PHONY: ecr-template
ecr-template: check
	@set -euo pipefail; \
	applied_for="$$(aws ecr describe-repository-creation-templates --region "$(AWS_REGION)" --prefixes "$(ROUTER_REPO_PREFIX)" --query 'repositoryCreationTemplates[0].appliedFor' --output text 2>/dev/null || true)"; \
	if [[ -z "$$applied_for" || "$$applied_for" == "None" ]]; then \
		echo "Creating ECR repository creation template for prefix $(ROUTER_REPO_PREFIX) (CREATE_ON_PUSH)"; \
		aws ecr create-repository-creation-template \
			--region "$(AWS_REGION)" \
			--prefix "$(ROUTER_REPO_PREFIX)" \
			--applied-for CREATE_ON_PUSH >/dev/null; \
	else \
		if echo "$$applied_for" | grep -q "CREATE_ON_PUSH"; then \
			echo "ECR repository creation template already includes CREATE_ON_PUSH ($$applied_for)"; \
		else \
			echo "Updating ECR repository creation template to include CREATE_ON_PUSH (was: $$applied_for)"; \
			aws ecr update-repository-creation-template \
				--region "$(AWS_REGION)" \
				--prefix "$(ROUTER_REPO_PREFIX)" \
				--applied-for $$applied_for CREATE_ON_PUSH >/dev/null; \
		fi; \
	fi

.PHONY: ecr-template-delete
ecr-template-delete: check
	aws ecr delete-repository-creation-template --region "$(AWS_REGION)" --prefix "$(ROUTER_REPO_PREFIX)" || true

.PHONY: ecr-login
ecr-login: check
	aws ecr get-login-password --region "$(AWS_REGION)" | docker login --username AWS --password-stdin "$(ECR_REGISTRY)"

.PHONY: image-build
image-build: check
	docker build --platform "$(ROUTER_IMAGE_PLATFORM)" -f Dockerfile.router -t "$(ROUTER_IMAGE_IDENTIFIER)" .

.PHONY: image-push
image-push: ecr-template ecr-login image-build
	docker push "$(ROUTER_IMAGE_IDENTIFIER)"

.PHONY: sam-build
sam-build:
	sam build --config-file sam/samconfig.toml --config-env default

.PHONY: sam-deploy
sam-deploy: sam-build
	sam deploy --config-file sam/samconfig.toml --config-env default

.PHONY: bootstrap-deploy
bootstrap-deploy: check
	@set -euo pipefail; \
	AWS_REGION="$(AWS_REGION)" AWS_DEFAULT_REGION="$(AWS_REGION)" \
		params=( \
			"DefaultRouterRepositoryName=$(ROUTER_REPO_NAME)" \
			"DefaultRouterImageTag=$(ROUTER_VERSION)" \
		); \
		if [[ -n "$(BOOTSTRAP_BUCKET)" ]]; then params+=("UseExistingBucket=$(BOOTSTRAP_BUCKET)"); fi; \
		sam deploy \
			--stack-name "$(BOOTSTRAP_STACK_NAME)" \
			--template-file "$(BOOTSTRAP_TEMPLATE)" \
			$(SAM_DEPLOY_FLAGS) \
			--parameter-overrides "$${params[@]}"

.PHONY: deploy
deploy: bootstrap-deploy image-push sam-deploy
