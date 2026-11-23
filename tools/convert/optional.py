# kept: 0
# Values that may be None.
from typing import Optional


class Item:
    def __init__(self, name, price):
        self.name = name
        self.price = price
        self.parent = None


def lookup(prices, name):
    return prices.get(name)


def cheapest(items):
    best = None
    for item in items:
        if best is None or item.price < best.price:
            best = item
    return best


def describe(item: Optional[Item]) -> str:
    if item is None:
        return "nothing"
    return item.name + " at " + str(item.price)


def depth(item):
    steps = 0
    current = item.parent
    while current is not None:
        steps += 1
        current = current.parent
    return steps


prices = {"tea": 3, "milk": 2}
found = lookup(prices, "tea")
if found is not None:
    print(found + 1)
missing = lookup(prices, "salt")
if missing is None:
    print("no salt")
if missing is not None and missing > 1:
    print("never")
items = [Item("tea", 3), Item("milk", 2), Item("rice", 5)]
best = cheapest(items)
print(describe(best), describe(None), describe(cheapest([])))
items[0].parent = items[1]
items[1].parent = items[2]
print(depth(items[0]), depth(items[2]))
limit = None
if len(items) > 2:
    limit = 10
if limit is not None:
    print(limit * 2)
