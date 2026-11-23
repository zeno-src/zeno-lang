// kept: all
/* The preprocessor decides what this file is, so it is kept whole. */
#include <stdio.h>

#ifndef LEVEL
#define LEVEL 2
#endif

int main(void) {
#if LEVEL > 1
    printf("level %d\n", LEVEL);
#endif
    return 0;
}
