#include <stdio.h>

void greet(char *name) {
    char buf[16];
    sprintf(buf, "hello %s", name);
    puts(buf);
}
