# kept: 0
# Classes whose objects are made of fields and methods.
from dataclasses import dataclass


class Counter:
    """Counts up."""

    def __init__(self, name, step):
        self.name = name
        self.step = step
        self.count = 0
        self.history = []

    def bump(self):
        self.count += self.step
        self.history.append(self.count)
        return self.count

    def reset(self):
        self.count = 0

    def describe(self):
        return f"{self.name}: {self.count}"


@dataclass
class Point:
    x: float
    y: float
    label: str = "p"

    def length(self):
        return (self.x ** 2 + self.y ** 2) ** 0.5

    def moved(self, dx, dy):
        return Point(self.x + dx, self.y + dy)


class Node:
    def __init__(self, value):
        self.value = value
        self.next = None


def total(counters):
    result = 0
    for counter in counters:
        result += counter.count
    return result


a = Counter("a", 1)
b = Counter("b", 10)
a.bump()
a.bump()
print(b.bump(), a.describe(), b.describe())
print(a.history, b.history, total([a, b]))
a.reset()
a.count = 5
a.count += 1
print(a.count, a.name)
p = Point(3.0, 4.0)
q = p.moved(1.0, 1.0)
print(p.length(), q.x, q.y, p.label, p == Point(3.0, 4.0), p == q)
head = Node(1)
head.next = Node(2)
node = head
while node is not None:
    print(node.value)
    node = node.next
