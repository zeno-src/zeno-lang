// kept: 0
/* switch, enum, do-while, break and continue. */
#include <stdio.h>
#include <stdbool.h>

enum Light { RED, AMBER = 5, GREEN };

const char *name_of(enum Light light) {
    switch (light) {
    case RED:
        return "red";
    case AMBER:
        return "amber";
    default:
        return "green";
    }
}

int score(char grade) {
    int points = 0;
    switch (grade) {
    case 'A':
    case 'a':
        points = 4;
        break;
    case 'B':
        points = 3;
        break;
    default:
        points = 0;
    }
    return points;
}

bool is_prime(int n) {
    if (n < 2) {
        return false;
    }
    for (int d = 2; d * d <= n; d++) {
        if (n % d == 0) {
            return false;
        }
    }
    return true;
}

int main(void) {
    printf("%s %s %s %d\n", name_of(RED), name_of(AMBER), name_of(GREEN), GREEN);
    printf("%d %d %d\n", score('a'), score('B'), score('F'));
    int found = 0;
    for (int n = 0; n < 50; n++) {
        if (!is_prime(n)) {
            continue;
        }
        found++;
        if (found > 10) {
            break;
        }
        printf("prime %d\n", n);
    }
    int k = 0;
    do {
        k += 7;
    } while (k % 5 != 0);
    printf("k %d\n", k);
    bool done = false;
    int rounds = 0;
    while (!done) {
        rounds++;
        done = rounds >= 3;
    }
    printf("rounds %d\n", rounds);
    return 0;
}
