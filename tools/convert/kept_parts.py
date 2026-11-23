# kept: 3
# Some of this cannot be Zeno, and is kept; the rest is written around it.
import json

LIMIT = 3


def squares(n):
    return [i * i for i in range(n)]


def evens(limit):
    # a generator, which Zeno does not have
    for i in range(limit):
        if i % 2 == 0:
            yield i


def label(n: int) -> str:
    return "n=" + str(n)


print(squares(LIMIT))
print(list(evens(LIMIT * 2)))
print(label(LIMIT), json.dumps({"a": 1}))
data = json.loads("[1, 2, 3]")
print(len(data), data[0], data)
