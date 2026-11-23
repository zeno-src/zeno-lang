# kept: 5
# Loops whose list is changed while they go over it. Python and Zeno do not
# agree on what such a loop sees, so the piece is kept. The other loops are
# Zeno, though their variables have the same names.
queue = [3, 1, 2]


def grow(item):
    if item > 1:
        queue.append(item - 1)


def total(items):
    result = 0
    for item in items:
        result += item
    return result


for item in queue:
    grow(item)
print(queue)

seen = []
for item in [4, 1, 3]:
    seen.append(item * 2)
print(seen, total(seen))

names = ["ann", "bob"]
for name in names:
    if len(names) < 4:
        names.append(name + "!")
print(names)

counts = {"a": 1, "b": 2}
for key, value in counts.items():
    print(key, value, total([value, value]))
for name in ["cy", "di"]:
    print(name.upper())
index = 0
while index < len(seen):
    if seen[index] > 4:
        seen.pop(index)
    else:
        index += 1
print(seen)
