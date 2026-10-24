CC ?= gcc
CFLAGS ?= -Wall -Wextra -O2 -g

all: test

test: wayland-wire.o test.o
	$(CC) $(CFLAGS) -o test wayland-wire.o test.o -lm
	./test

wayland-wire.o: wayland-wire.c wayland-wire.h
	$(CC) $(CFLAGS) -c wayland-wire.c

test.o: test.c wayland-wire.h
	$(CC) $(CFLAGS) -c test.c

clean:
	rm -f *.o test

.PHONY: all test clean
