// kept: 0
/* float has 32 bits, double 64; the functions of math.h. */
#include <stdio.h>
#include <math.h>

float average(const float values[], int count) {
    float total = 0.0f;
    for (int i = 0; i < count; i++) {
        total += values[i];
    }
    return total / count;
}

double hypotenuse(double a, double b) {
    return sqrt(a * a + b * b);
}

int main(void) {
    float readings[4] = {0.1f, 0.2f, 0.3f, 0.4f};
    printf("%.10f\n", average(readings, 4));
    float step = 0.1f;
    float sum = 0;
    for (int i = 0; i < 10; i++) {
        sum += step;
    }
    printf("%.9f %.9f\n", sum, 0.1 * 10);
    printf("%.4f %.1f %.1f %.1f\n", hypotenuse(3, 4), floor(-2.5), ceil(2.1), round(2.5));
    printf("%.6f %.3f\n", pow(2, 0.5), fabs(-3.25));
    double celsius = 37.5;
    int whole = (int)celsius;
    printf("%d %.2f\n", whole, celsius * 9 / 5 + 32);
    int n = 7;
    double half = n / 2;
    double real_half = n / 2.0;
    printf("%.1f %.1f\n", half, real_half);
    return 0;
}
