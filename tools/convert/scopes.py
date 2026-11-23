# kept: 0
# Where a name lives: Python has one set of names for a function, and
# Zeno one for each block.
count = 3


def classify(n):
    if n > 10:
        size = "big"
        extra = n - 10
    else:
        size = "small"
        extra = 0
    return size + " " + str(extra)


def first_even(numbers):
    found = -1
    for n in numbers:
        if n % 2 == 0:
            found = n
            break
    return found


def tally(words):
    longest = ""
    total = 0
    for word in words:
        length = len(word)
        total += length
        if length > len(longest):
            longest = word
    return longest, total


def shadow(count):
    count = count + 1
    return count


def uses_global():
    return count * 2


def changes_global():
    global count
    count += 1


def loops():
    result = []
    for i in range(3):
        result.append(i)
    for i in range(2):
        result.append(i * 10)
    i = 99
    result.append(i)
    return result


def nested(limit):
    rows = []
    for i in range(limit):
        row = []
        for j in range(i + 1):
            row.append(i * j)
        rows.append(row)
    return rows


def maybe(flag):
    if flag:
        message = "yes"
    else:
        message = "no"
    while flag:
        message = message + "!"
        flag = False
    return message


print(classify(15), classify(5))
print(first_even([1, 3, 6, 8]), first_even([1, 3]))
longest, total = tally(["a", "ccc", "bb"])
print(longest, total)
print(shadow(1), count, uses_global())
changes_global()
print(count, uses_global())
print(loops(), nested(3))
print(maybe(True), maybe(False))
if count > 2:
    label = "many"
else:
    label = "few"
print(label)
for i in range(2):
    square = i * i
    print(square)
size = 0
for word in ["x", "yy"]:
    size += len(word)
print(size)
