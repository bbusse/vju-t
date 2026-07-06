HASH   := $(shell git rev-parse --short HEAD)
REMOTE ?= gh

.PHONY: release release-candidate rc _check-remote

_check-remote:
	@git remote get-url $(REMOTE) > /dev/null 2>&1 || \
	    { echo "Error: no remote '$(REMOTE)' — add one with: git remote add $(REMOTE) <url>"; exit 1; }

release: _check-remote
	$(eval TAG := release-$(HASH))
	git tag -f $(TAG)
	@printf 'Tagged %s as %s\n' "$(HASH)" "$(TAG)"
	@printf 'Push tag to trigger a release? [y/N] ' && read ans && \
	    case "$$ans" in [yY]) git push $(REMOTE) $(TAG) ;; \
	    *) git tag -d $(TAG); echo 'Aborted — tag removed.' ;; esac

release-candidate rc: _check-remote
	$(eval TAG := rc-$(HASH))
	git tag -f $(TAG)
	@printf 'Tagged %s as %s\n' "$(HASH)" "$(TAG)"
	@printf 'Push tag to trigger a release candidate? [y/N] ' && read ans && \
	    case "$$ans" in [yY]) git push $(REMOTE) $(TAG) ;; \
	    *) git tag -d $(TAG); echo 'Aborted — tag removed.' ;; esac
