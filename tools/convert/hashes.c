// kept: 0
/* 64-bit numbers that go round: a hash, two random generators, and a
 * counter of microseconds, beside sizes and ints. */
#include <stdio.h>
#include <stdint.h>
#include <string.h>

#define FNV_OFFSET 0xcbf29ce484222325ULL
#define FNV_PRIME 0x100000001b3ULL

uint64_t fnv1a(const char *text) {
    uint64_t hash = FNV_OFFSET;
    size_t length = strlen(text);
    for (size_t i = 0; i < length; i++) {
        hash ^= (unsigned char)text[i];
        hash *= FNV_PRIME;
    }
    return hash;
}

uint64_t state = 88172645463325252ULL;

uint64_t xorshift(void) {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    return state;
}

uint64_t splitmix(uint64_t *seed) {
    *seed += 0x9E3779B97F4A7C15ULL;
    uint64_t z = *seed;
    z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ULL;
    z = (z ^ (z >> 27)) * 0x94D049BB133111EBULL;
    return z ^ (z >> 31);
}

uint32_t low_half(uint64_t value) {
    return (uint32_t)value;
}

int dice(uint64_t random) {
    return (int)(random % 6) + 1;
}

int main(void) {
    const char *words[] = {"", "a", "zeno", "firmware"};
    for (int i = 0; i < 4; i++) {
        printf("%-9s %016lx\n", words[i], fnv1a(words[i]));
    }
    for (int i = 0; i < 3; i++) {
        uint64_t r = xorshift();
        printf("%lu %u %d\n", r, low_half(r), dice(r));
    }
    uint64_t seed = 42;
    printf("%lu\n", splitmix(&seed));
    printf("%lu\n", splitmix(&seed));
    uint64_t micros = 4000000000ULL;
    micros += 300;
    uint64_t back = micros - 5000000000ULL;
    printf("%lu %lu %lu\n", micros, back, micros / 1000);
    unsigned long mask = ~0UL;
    printf("%lx %lu\n", mask, mask >> 60);
    double seconds = (double)micros / 1e6;
    printf("%.3f\n", seconds);
    return 0;
}
