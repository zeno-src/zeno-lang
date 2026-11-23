# kept: 0
# if, while, for, break and continue.
n = 27
steps = 0
while n != 1:
    if n % 2 == 0:
        n = n // 2
    else:
        n = 3 * n + 1
    steps += 1
print(steps)
for i in range(10):
    if i == 3:
        continue
    if i == 6:
        break
    print(i)
for i in range(2, 11, 4):
    print(i)
for i in range(5, 0, -2):
    print(i)
found = False
for i in range(100):
    if i * i > 50:
        found = True
        break
print(found)
grade = 71
if grade >= 90:
    letter = "A"
elif grade >= 70:
    letter = "B"
else:
    letter = "C"
print(letter)
count = 0
while True:
    count += 1
    if count >= 5:
        break
print(count)
limit = 3
label = "big" if limit > 2 else "small"
print(label)
i = 0
while i < 3:
    j = 0
    while j < 2:
        print(i, j)
        j += 1
    i += 1
pass
