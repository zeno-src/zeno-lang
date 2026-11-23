// kept: 2
/* Pointers to one number, and arrays given to functions. */
#include <stdio.h>

void swap(int *a, int *b) {
    int t = *a;
    *a = *b;
    *b = t;
}

void min_max(const int values[], int count, int *low, int *high) {
    *low = values[0];
    *high = values[0];
    for (int i = 1; i < count; i++) {
        if (values[i] < *low) *low = values[i];
        if (values[i] > *high) *high = values[i];
    }
}

void sort(int values[], int count) {
    for (int i = 0; i < count; i++) {
        for (int j = 0; j + 1 < count - i; j++) {
            if (values[j] > values[j + 1]) {
                swap(&values[j], &values[j + 1]);
            }
        }
    }
}

void bubble(int values[], int count) {
    for (int i = 0; i < count; i++) {
        for (int j = 0; j + 1 < count - i; j++) {
            if (values[j] > values[j + 1]) {
                int t = values[j];
                values[j] = values[j + 1];
                values[j + 1] = t;
            }
        }
    }
}

int sum(const int *values, int count) {
    int total = 0;
    for (int i = 0; i < count; i++) {
        total += values[i];
    }
    return total;
}

int main(void) {
    int x = 1, y = 2;
    swap(&x, &y);
    printf("x=%d y=%d\n", x, y);
    int values[] = {5, -3, 9, 0, 4};
    int count = sizeof values / sizeof values[0];
    int low, high;
    min_max(values, count, &low, &high);
    printf("low=%d high=%d sum=%d\n", low, high, sum(values, count));
    bubble(values, count);
    for (int i = 0; i < count; i++) {
        printf("%d\n", values[i]);
    }
    int matrix[3][3] = {{1, 2, 3}, {4, 5, 6}, {7, 8, 9}};
    int trace = 0;
    for (int i = 0; i < 3; i++) {
        trace += matrix[i][i];
    }
    printf("trace %d\n", trace);
    return 0;
}
