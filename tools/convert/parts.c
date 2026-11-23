// kept: 0
/* Lines written in parts. */
#include <stdio.h>

void row(int n) {
    for (int i = 0; i < n; i++) {
        putchar('*');
    }
    putchar('\n');
}

int main(void) {
    for (int n = 1; n <= 4; n++) {
        row(n);
    }
    int values[4] = {3, 1, 4, 1};
    printf("values:");
    for (int i = 0; i < 4; i++) {
        printf(" %d", values[i]);
    }
    printf("\n");
    printf("two\nlines\n");
    printf("no end");
    return 0;
}
