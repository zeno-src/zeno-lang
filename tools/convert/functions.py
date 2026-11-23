# kept: 0
# Functions: defaults, names for arguments, several values, recursion.
def add(a, b):
    return a + b


def greet(name, greeting="hello", times=1):
    for _ in range(times):
        print(greeting, name)


def divide(a: int, b: int):
    return a // b, a % b


def factorial(n):
    if n <= 1:
        return 1
    return n * factorial(n - 1)


def sign(n):
    return "minus" if n < 0 else "plus"


def find(items, wanted):
    for index, item in enumerate(items):
        if item == wanted:
            return index
    return None


def describe(value: float) -> str:
    """Say how large a value is."""
    if value > 100:
        size = "large"
    elif value > 10:
        size = "medium"
    else:
        size = "small"
    return size


def show(items):
    if not items:
        print("nothing")
        return
    for item in items:
        print(item)


print(add(1, 2), add(3, 4))
greet("ann")
greet("bob", "hi")
greet("cy", times=2)
greet(greeting="hey", name="dee")
quotient, rest = divide(17, 5)
print(quotient, rest)
print(factorial(10), sign(-3), sign(3))
where = find(["a", "b", "c"], "b")
if where is not None:
    print("found at", where + 1)
if find(["a"], "z") is None:
    print("not found")
print(describe(500.0), describe(50.0), describe(5.0))
show([])
show([1, 2])
a, b = 1, 2
a, b = b, a
print(a, b)
