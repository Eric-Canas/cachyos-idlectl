# Man pages, written in scdoc.
#
# Precedent for scdoc over raw roff: greetd. The source is readable enough that a
# behaviour change and its documentation can be made in the same commit without
# anyone having to remember roff.
#
# Invoked by the top-level Makefile as `make -C man`. Nothing here is generated from
# the binaries, so a cross-build never has to run a freshly compiled program.

SCDOC ?= scdoc

PAGES = idlectl.1 idlectl.toml.5 idlepolicyd.8

.PHONY: all clean

all: $(PAGES)

# Written out one rule per page rather than as a pattern rule: `%: %.scd` is a
# match-anything rule in GNU make and interacts badly with the implicit rule search.
idlectl.1: idlectl.1.scd
	$(SCDOC) < $< > $@

idlectl.toml.5: idlectl.toml.5.scd
	$(SCDOC) < $< > $@

idlepolicyd.8: idlepolicyd.8.scd
	$(SCDOC) < $< > $@

clean:
	rm -f $(PAGES)
