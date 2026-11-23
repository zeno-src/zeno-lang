# kept: 1
# What Python has and Zeno has not, written out as small functions of Zeno:
# sorted, remove, split with a number, a text times a number. And the rest
# of what came with them: a if c else b, a loop's variable used after the
# loop, rows written out, a generator in sum().
from dataclasses import dataclass


@dataclass
class Item:
    name: str
    price: float
    count: int = 1


def total(items):
    return sum(item.price * item.count for item in items)


def label(item):
    kind = "many" if item.count > 1 else "one"
    return f"{item.name}: {kind}"


def pick(values, wanted):
    found = values[0] if len(values) > 0 else -1
    for value in values:
        if value == wanted:
            found = value
    return found


numbers = [5, 3, 9, 1]
print(sorted(numbers), numbers, sorted([]), sorted(["b", "a", "c"]))
scores = {"zed": 3, "amy": 9, "bob": 1}
print(sorted(scores), max(len(name) for name in scores), min(scores[key] for key in scores))
print(", ".join(sorted(name.upper() for name in scores)))
numbers.remove(9)
print(numbers)
try:
    numbers.remove(42)
except ValueError as error:
    print("not there")
print("a b c d".split(" ", 2), "a b".split(" ", 0), "abc".split(" ", 5), "a,b,,c".split(",", 2))
print("-" * 10, "[" + "ab" * 0 + "]", "[" + "x" * -3 + "]", 3 * "ok")
items = [Item("tea", 2.5, 4), Item("cake", 3.0), Item(name="jam", price=1.25, count=2)]
print(total(items), label(items[0]), label(items[1]))
print(pick([4, 8, 15], 8), pick([], 1), pick([7], 3))
for name, price in [("bread", 2.0), ("milk", 1.5)]:
    print(name, price)
for name, price in [("bread", 2), ("milk", 1.5)]:
    print(name, price)
for word in ["first", "second", "last"]:
    pass
print("the loop ended at", word)
for n in []:
    print("never")
count = 0
for line in ["x", "", "y"]:
    if not line:
        continue
    count += 1
print(count, line)
