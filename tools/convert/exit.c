// kept: 0
/* The code a program ends with. */
#include <stdio.h>
#include <stdlib.h>

void check(int value) {
    if (value > 100) {
        fprintf(stderr, "too big: %d\n", value);
        exit(2);
    }
}

int main(void) {
    check(5);
    printf("five is fine\n");
    int total = 0;
    for (int i = 0; i < 5; i++) {
        total += i;
    }
    if (total != 10) {
        return 1;
    }
    printf("total %d\n", total);
    return 3;
}
