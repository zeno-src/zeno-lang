# kept: 0
# The program is the function `main`, called at the end.
import sys
import math

total = 0
history = []


def record(value):
    global total
    total += value
    history.append(value)


def mean():
    if len(history) == 0:
        return 0.0
    return total / len(history)


def main():
    for value in [3, 4, 5]:
        record(value)
    print(total, mean(), math.sqrt(16), math.floor(2.7), math.ceil(2.1))
    print(len(sys.argv[1:]))
    if total > 100:
        sys.exit(1)
    print("done")


if __name__ == "__main__":
    main()
