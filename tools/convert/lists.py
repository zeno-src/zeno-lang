# kept: 0
# What lists do.
numbers = [5, 3, 8, 1]
print(numbers, len(numbers), numbers[0], numbers[-1], numbers[1:3])
numbers.append(7)
numbers.insert(0, 9)
print(numbers)
last = numbers.pop()
first = numbers.pop(0)
print(last, first, numbers)
numbers.sort()
print(numbers)
numbers.reverse()
print(numbers)
copy = numbers.copy()
copy.append(100)
other = numbers[:]
other.append(200)
same = numbers
same.append(300)
print(numbers, copy, other)
print(3 in numbers, 4 in numbers, 4 not in numbers)
squares = [n * n for n in numbers]
evens = [n for n in numbers if n % 2 == 0]
print(squares, evens, [i * 2 for i in range(4)])
words = []
for word in ["pear", "fig", "apple"]:
    words.append(word)
ordered = sorted(words)
print(words, ordered)
joined = numbers + [1, 2]
print(joined, len(joined))
grid = [[1, 2], [3, 4]]
grid[0][1] = 20
grid.append([5, 6])
print(grid, grid[1][0], len(grid))
total = 0
for row in grid:
    for cell in row:
        total += cell
print(total)
for index, word in enumerate(words):
    print(index, word)
if numbers:
    print("some numbers")
nothing = []
if not nothing:
    print("nothing there")
numbers.clear()
print(numbers, len(numbers))
print(sum(squares), max(squares), min(squares))
