// kept: 0
/* Sums, quotients and the ways printf shows a number. */
#include <stdio.h>

int square(int n) {
    return n * n;
}

/* The sum of 1 to n. */
long triangle(int n) {
    long total = 0;
    for (int i = 1; i <= n; i++) {
        total += i;
    }
    return total;
}

int gcd(int a, int b) {
    while (b != 0) {
        int t = a % b;
        a = b;
        b = t;
    }
    return a;
}

int main(void) {
    printf("%d %d %ld\n", square(7), square(-3), triangle(100));
    printf("gcd %d\n", gcd(84, 36));
    // C takes the quotient toward zero.
    int a = -7, b = 2;
    printf("%d %d %d %d\n", a / b, a % b, 7 / -2, 7 % -2);
    printf("[%5d] [%-5d] [%05d] [%x] [%X]\n", 42, 42, 42, 255, 3054);
    printf("[%8.3f] [%-8.2f] [%.0f] [%f]\n", 3.14159, 2.5, 2.5, 1.0 / 3.0);
    printf("[%10s] [%-6s] %c%c 100%%\n", "right", "left", 'o', 'k');
    int count = 0;
    for (int i = 10; i > 0; i -= 3) {
        count++;
        printf("i=%d\n", i);
    }
    printf("count %d\n", count);
    int n = 27, steps = 0;
    while (n != 1) {
        if (n % 2 == 0) {
            n = n / 2;
        } else {
            n = 3 * n + 1;
        }
        steps++;
    }
    printf("27 reaches 1 in %d steps\n", steps);
    return 0;
}
