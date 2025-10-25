ANSIBLE ?= ansible-playbook
INVENTORY ?= inventories/prod

.PHONY: deploy deploy-local ansible-check ansible-lint

deploy:
$(ANSIBLE) -i $(INVENTORY) playbooks/site.yml

deploy-local:
$(ANSIBLE) -i inventories/local playbooks/site.yml -e orchestration_skip_deploy=false

ansible-check:
$(ANSIBLE) -i inventories/local playbooks/site.yml --check -e orchestration_skip_deploy=true

ansible-lint:
ansible-lint playbooks/site.yml
