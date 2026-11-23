// kept: 2
/* Names for values, macros, and globals. */
#include <stdio.h>

#define SIZE 5
#define GREETING "hello"
#define SQUARE(x) ((x) * (x))

const int LIMIT = 3;
int calls = 0;
int history[SIZE];

void remember(int value) {
    history[calls % SIZE] = value;
    calls++;
}

int area(int side) {
    return SQUARE(side);
}

int main(void) {
    for (int i = 0; i < 7; i++) {
        remember(i * LIMIT);
    }
    printf("%s, %d calls\n", GREETING, calls);
    for (int i = 0; i < SIZE; i++) {
        printf("%d ", history[i]);
    }
    printf("\n");
    printf("area %d\n", area(SIZE));
    return 0;
}
