// kept: 0
/* Unsigned sums go round, as checksums and hashes need. */
#include <stdio.h>
#include <stdint.h>

#define FNV_PRIME 16777619u

uint32_t djb2(const char *text) {
    uint32_t hash = 5381;
    for (int i = 0; text[i] != '\0'; i++) {
        hash = hash * 33 + text[i];
    }
    return hash;
}

uint32_t fnv1a(const char *text) {
    uint32_t hash = 2166136261u;
    for (int i = 0; text[i]; i++) {
        hash ^= (uint8_t)text[i];
        hash *= FNV_PRIME;
    }
    return hash;
}

uint8_t checksum(const uint8_t *data, int length) {
    uint8_t sum = 0;
    for (int i = 0; i < length; i++) {
        sum += data[i];
    }
    return (uint8_t)~sum;
}

uint16_t crc16(const uint8_t *data, int length) {
    uint16_t crc = 0xFFFF;
    for (int i = 0; i < length; i++) {
        crc ^= data[i];
        for (int bit = 0; bit < 8; bit++) {
            if (crc & 1) {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    return crc;
}

unsigned mix(unsigned a, unsigned b) {
    return a * b + (a << 13) - (b >> 3);
}

int ones(unsigned x) {
    int count = 0;
    while (x != 0) {
        x &= x - 1;
        count++;
    }
    return count;
}

int main(void) {
    printf("%u %u\n", djb2("hello"), djb2("a longer text to hash"));
    printf("%08x\n", fnv1a("zeno"));
    uint8_t frame[6] = {0x01, 0x03, 0x00, 0x6B, 0x00, 0x03};
    printf("checksum %u crc %u\n", checksum(frame, 6), crc16(frame, 6));
    printf("mix %u %u\n", mix(4000000000u, 3000000000u), mix(1, 2));
    printf("ones %d %d\n", ones(0xFFu), ones(0x80000001u));
    unsigned small = 3;
    printf("%u\n", small - 5);
    int negative = -1;
    printf("%u %x\n", negative, negative);
    return 0;
}
