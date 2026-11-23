// kept: 0
/* Primes by the sieve, a table, and a binary search. */
#include <stdio.h>
#include <stdbool.h>

#define LIMIT 100

bool composite[LIMIT + 1];
int primes[LIMIT];
int count = 0;

void sieve(void) {
    for (int n = 2; n <= LIMIT; n++) {
        if (composite[n]) {
            continue;
        }
        primes[count++] = n;
        for (int m = n * n; m <= LIMIT; m += n) {
            composite[m] = true;
        }
    }
}

int find(int value) {
    int low = 0, high = count - 1;
    while (low <= high) {
        int middle = (low + high) / 2;
        if (primes[middle] == value) {
            return middle;
        } else if (primes[middle] < value) {
            low = middle + 1;
        } else {
            high = middle - 1;
        }
    }
    return -1;
}

int main(void) {
    sieve();
    printf("%d primes up to %d\n", count, LIMIT);
    for (int row = 0; row < count; row += 10) {
        for (int i = row; i < row + 10 && i < count; i++) {
            printf("%4d", primes[i]);
        }
        printf("\n");
    }
    printf("97 is at %d, 91 at %d\n", find(97), find(91));
    return 0;
}
