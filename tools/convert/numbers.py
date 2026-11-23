# kept: 0
# What numbers do: the operators, whole numbers and floats together.
a = 17
b = 5
print(a + b, a - b, a * b, a // b, a % b, a ** 2)
print(a / b, -a // b, -a % b, a % -b)
x = 2.5
print(x * 2, x + a, x / 2, x // 1, x % 2, x ** 2)
print(a == 17, a != b, a < b, a >= b, 1 < a < 20, 1 < a < 10)
print(a & b, a | b, a ^ b, a << 2, a >> 1, ~a)
print(abs(-3), abs(-2.5), min(a, b), max(a, b), min(1.5, 2.5))
print(int(3.9), int(-3.9), float(3), int("42"), float("2.5"))
print(0x1f, 0b101, 0o17, 1_000_000, 1e3, 2.5e-3)
print(sum([1, 2, 3]), sum([0.5, 1.5]), max([3, 9, 2]), min([3, 9, 2]))
print(-a, +a, - -a, a - -b, 2 ** 3 ** 2, (2 ** 3) ** 2, -2 ** 2)
print(7 // 2 * 2 + 7 % 2, (a + b) * (a - b), a + b * a - b)
print(not a > b, a > b and b > 0, a < b or b > 0)
total = 0
total += 5
total *= 3
total -= 1
total //= 2
total %= 5
total **= 2
print(total)
ratio = 1
ratio = ratio / 4
print(ratio)
