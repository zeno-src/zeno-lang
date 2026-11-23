# kept: 9
# Pieces that cannot be Zeno, with pieces that can between them. What is
# printed must come in the order of the file.
import os.path

print("one")
with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "mixed.py")) as file:
    first = file.readline().strip()
print("two", first)
total = 0
for n in (1, 2, 3):
    total += n
print("three", total)
squares = {n: n * n for n in range(4)}
print("four", len(squares), squares[3])
text = "x=%d" % total
print("five", text)


def helper(a, *rest):
    return a + len(rest)


print("six", helper(1, 2, 3))
values = [3, 1, 2]
values.sort(key=lambda v: -v)
print("seven", values[0])
