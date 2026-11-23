# kept: 0
# Errors that are raised and caught.
def parse(text):
    try:
        return int(text)
    except ValueError:
        return -1


def check(n):
    if n < 0:
        raise ValueError("negative")
    if n > 100:
        raise OverflowError("too large")
    return n


def safe_divide(a, b):
    try:
        return a // b
    except ZeroDivisionError:
        print("cannot divide by zero")
        return 0


print(parse("12"), parse("x"))
for n in [5, -1, 500]:
    try:
        print(check(n))
    except ValueError as error:
        print("value:", error)
    except Exception as error:
        print("other:", error)
print(safe_divide(7, 2), safe_divide(7, 0))
items = [1, 2, 3]
try:
    print(items[10])
except IndexError:
    print("no such item")
ages = {"ann": 1}
try:
    print(ages["bob"])
except KeyError:
    print("no such key")
try:
    try:
        raise ValueError("inner")
    except ValueError as error:
        print("caught", error)
        raise
except ValueError as error:
    print("again", error)
try:
    raise Exception("plain")
except Exception as error:
    print(error)
