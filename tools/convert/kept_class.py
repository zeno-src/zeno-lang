# kept: 3
# A class made from another class is kept, and Zeno holds its objects
# and calls them without looking inside.
import random


class Shape:
    def __init__(self, name):
        self.name = name

    def area(self):
        return 0.0

    def __str__(self):
        return f"{self.name} of area {self.area():.1f}"


class Square(Shape):
    def __init__(self, side):
        super().__init__("square")
        self.side = side

    def area(self):
        return self.side * self.side


registry = []
banner = """shapes
  and their areas"""


def register(shape):
    registry.append(shape)
    return len(registry)


def scale(factor: int) -> int:
    return factor * 2


print(banner)
square = Square(3)
print(register(square), register(Square(2)))
print(square)
print(square.name, square.side, square.area())
for shape in registry:
    print(str(shape))
random.seed(7)
doubled = scale(4) + 1
print(doubled)
count = 0
for n in [1, 2, 3]:
    count += scale(n)
print(count)
if __name__ == "__main__":
    print("main", len(registry))
