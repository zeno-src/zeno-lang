// kept: 0
/* A small piece of firmware: a ring buffer of bytes, a frame parser and
 * a state machine, as they are written for a board. */
#include <stdio.h>
#include <stdint.h>
#include <stdbool.h>

#define RING_SIZE 8
#define START_BYTE 0xA5

typedef enum { WAIT_START, READ_LENGTH, READ_BODY, READ_SUM } State;

static uint8_t ring[RING_SIZE];
static uint8_t head = 0;
static uint8_t tail = 0;
static uint32_t dropped = 0;

bool ring_put(uint8_t byte) {
    uint8_t next = (head + 1) % RING_SIZE;
    if (next == tail) {
        dropped++;
        return false;
    }
    ring[head] = byte;
    head = next;
    return true;
}

bool ring_get(uint8_t *byte) {
    if (head == tail) {
        return false;
    }
    *byte = ring[tail];
    tail = (tail + 1) % RING_SIZE;
    return true;
}

static State state = WAIT_START;
static uint8_t length = 0;
static uint8_t got = 0;
static uint8_t sum = 0;
static uint8_t body[16];

/* Feed one byte; gives true when a whole frame has come. */
bool feed(uint8_t byte) {
    switch (state) {
    case WAIT_START:
        if (byte == START_BYTE) {
            state = READ_LENGTH;
        }
        break;
    case READ_LENGTH:
        length = byte;
        got = 0;
        sum = 0;
        state = length > 0 && length <= sizeof body ? READ_BODY : WAIT_START;
        break;
    case READ_BODY:
        body[got] = byte;
        got++;
        sum += byte;
        if (got == length) {
            state = READ_SUM;
        }
        break;
    case READ_SUM:
        state = WAIT_START;
        return (uint8_t)(sum + byte) == 0;
    }
    return false;
}

int main(void) {
    const uint8_t stream[] = {0x00, 0xA5, 0x03, 0x10, 0x20, 0x30, 0xA0, 0xA5, 0x02, 0xFF, 0x01, 0x00, 0xA5, 0x01, 0x07, 0x00};
    int frames = 0;
    for (unsigned i = 0; i < sizeof stream; i++) {
        if (!ring_put(stream[i])) {
            uint8_t byte;
            while (ring_get(&byte)) {
                if (feed(byte)) {
                    frames++;
                    printf("frame of %u bytes, first 0x%02X\n", length, body[0]);
                }
            }
            ring_put(stream[i]);
        }
    }
    uint8_t byte;
    while (ring_get(&byte)) {
        if (feed(byte)) {
            frames++;
            printf("frame of %u bytes, first 0x%02X\n", length, body[0]);
        }
    }
    printf("%d frames, %u bytes dropped\n", frames, dropped);
    return 0;
}
