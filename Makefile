MAN_SRC := $(wildcard docs/man/src/*.md)
MAN_OUT := $(patsubst docs/man/src/%.md,docs/man/%.1,$(MAN_SRC))

.PHONY: man clean

man: $(MAN_OUT)

docs/man/%.1: docs/man/src/%.md
	pandoc -s -t man $< -o $@

clean:
	rm -f $(MAN_OUT)
