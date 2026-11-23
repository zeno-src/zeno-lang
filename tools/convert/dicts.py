# kept: 0
# What dicts do.
ages = {"ann": 31, "bob": 27}
ages["cy"] = 45
ages["ann"] += 1
print(ages["ann"], ages.get("bob"), ages.get("dee", 0), len(ages))
print("ann" in ages, "dee" in ages, "dee" not in ages)
for name in ages:
    print(name, ages[name])
for name, age in ages.items():
    print(f"{name} is {age}")
for age in ages.values():
    print(age)
for name in ages.keys():
    print(name)
names = list(ages.keys())
values = list(ages.values())
print(names, values)
del ages["bob"]
gone = ages.pop("cy")
print(gone, len(ages))
counts = {}
for letter in "mississippi":
    counts[letter] = counts.get(letter, 0) + 1
for letter, count in counts.items():
    print(letter, count)
by_number = {1: "one", 2: "two"}
by_number[3] = "three"
print(by_number[2], 3 in by_number)
nested = {"a": [1, 2], "b": []}
nested["b"].append(3)
print(nested["a"][1], nested["b"], len(nested["a"]))
if counts:
    print("counted")
empty = {}
if not empty:
    print("nothing counted")
