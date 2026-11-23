# kept: 0
# What texts do.
name = "Zeno"
greeting = 'hello, ' + name
print(greeting, len(greeting))
print(greeting.upper(), greeting.lower())
print("  padded  ".strip() + "|")
print("a,b,c".split(","), "one two  three".split())
print("-".join(["x", "y", "z"]), "abc".replace("b", "B"))
print(name.startswith("Ze"), name.endswith("x"), "en" in name, "q" not in name)
print(name[0], name[1:3], name[2:], name[:2], name[-1], name[-2:])
for letter in name:
    print(letter)
print("tab\there", "quote\"d", 'single\'s', "back\\slash")
print("line one\nline two")
print(f"{name} has {len(name)} letters")
print(f"{name:>8}|{name:<8}|{name:^8}|")
count = 42
price = 3.14159
print(f"{count:5d}|{count:<5d}|{count:05d}|{count:x}|{count:X}|{count:b}")
print(f"{price:.2f}|{price:8.3f}|{price:<8.1f}|{count:.1f}")
print(f"{{braces}} and {count + 1} and {name.upper()}")
print(f"{'inner'} {name[0]} {len('abc')}")
print(str(count) + "!", str(price), "n=" + str(count))
print("a" < "b", "apple" < "banana", "a" == "a", name != "zeno")
text = """first
second
third"""
print(text.splitlines())
print(chr(65), ord("a"), chr(ord("a") + 1))
empty = ""
if not empty:
    print("empty is false")
if name:
    print("name is true")
long = name + "!"
long += "?"
print(long)
