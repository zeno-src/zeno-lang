# kept: 0
# Parts of texts and of lists, with ends past the end, as Python cuts them.
word = "tortoise"
print(word[0:3], word[3:], word[:4])
print(word[2:100], "[" + word[50:] + "]", word[:50])
print(word[-3:], word[:-3], word[-100:])
short = "ab"
print("[" + short[5:9] + "]", short[-5:], short[:-5] + "|")
numbers = [1, 2, 3, 4, 5]
print(numbers[1:3], numbers[3:], numbers[:2])
print(numbers[2:99], numbers[9:], numbers[:99])
print(numbers[-2:], numbers[:-2], numbers[-99:])


def head(items, count):
    return items[:count]


def tail(text, count):
    return text[len(text) - count:]


def middle(text, start, end):
    return text[start:end]


print(head(numbers, 3), head(numbers, 10), head([], 2))
print(tail("zeno", 2), middle("language", 2, 5), middle("ab", 1, 40))
start = 1
for size in range(0, 8, 3):
    print(size, word[start:start + size], numbers[start:start + size])
copy = numbers[:]
copy.append(6)
print(len(numbers), len(copy))
lines = ["a", "b", "c", "d"]
first, rest = lines[0], lines[1:]
print(first, len(rest), rest[0])
