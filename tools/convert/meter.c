// kept: 1
/* A meter: readings, an average, and a checksum. */
#include <stdio.h>
#include <stdint.h>

#define COUNT 5

struct Reading {
    const char *name;
    int value;
};

uint8_t checksum(const uint8_t *bytes, int count) {
    uint8_t sum = 0;
    for (int i = 0; i < count; i++) {
        sum += bytes[i];
    }
    return (uint8_t)~sum;
}

int average(const struct Reading readings[], int count) {
    int total = 0;
    for (int i = 0; i < count; i++) {
        total += readings[i].value;
    }
    return total / count;
}

void limits(const struct Reading readings[], int count, int *low, int *high) {
    *low = readings[0].value;
    *high = readings[0].value;
    for (int i = 1; i < count; i++) {
        if (readings[i].value < *low) *low = readings[i].value;
        if (readings[i].value > *high) *high = readings[i].value;
    }
}

uint32_t tick(void) {
    static uint32_t count = 0;
    return ++count;
}

int main(void) {
    struct Reading readings[COUNT] = {
        {"volts", 230}, {"amps", 12}, {"hertz", 50}, {"watts", 2760}, {"temp", -4},
    };
    for (int i = 0; i < COUNT; i++) {
        printf("%-6s %5d\n", readings[i].name, readings[i].value);
    }
    int low, high;
    limits(readings, COUNT, &low, &high);
    printf("average %d, low %d, high %d\n", average(readings, COUNT), low, high);
    uint8_t frame[] = {0x10, 0x20, 0xF0};
    printf("checksum 0x%02X\n", checksum(frame, sizeof frame));
    uint32_t first = tick();
    printf("tick %u, tick %u\n", first, tick());
    return 0;
}
