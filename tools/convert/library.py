# kept: 1
# A small library: books, members, loans, and a report at the end. A program
# of the length people do write, with the usual mix in it.
import json
import math
import sys
from dataclasses import dataclass

DAYS_ALLOWED = 14
FINE_PER_DAY = 0.25
MAX_LOANS = 3


@dataclass
class Book:
    title: str
    author: str
    year: int
    copies: int = 1


class Member:
    def __init__(self, name, joined):
        self.name = name
        self.joined = joined
        self.loans = []
        self.fines = 0.0

    def can_borrow(self):
        return len(self.loans) < MAX_LOANS and self.fines < 5.0

    def describe(self):
        return f"{self.name} (since {self.joined}, {len(self.loans)} on loan)"


class Loan:
    def __init__(self, title, member, day):
        self.title = title
        self.member = member
        self.day = day
        self.returned = -1

    def days_late(self, today):
        end = today if self.returned < 0 else self.returned
        late = end - self.day - DAYS_ALLOWED
        return late if late > 0 else 0


books = {}
members = {}
loans = []
log = []


def add_book(title, author, year, copies=1):
    if title in books:
        books[title].copies += copies
    else:
        books[title] = Book(title, author, year, copies)


def add_member(name, joined):
    if name in members:
        raise ValueError("there is a member " + name + " already")
    members[name] = Member(name, joined)


def available(title):
    out = 0
    for loan in loans:
        if loan.title == title and loan.returned < 0:
            out += 1
    return books[title].copies - out


def borrow(name, title, day):
    member = members[name]
    if title not in books:
        raise KeyError(title)
    if not member.can_borrow():
        log.append(f"day {day}: {name} may not borrow {title}")
        return False
    if available(title) == 0:
        log.append(f"day {day}: no copy of {title} for {name}")
        return False
    loans.append(Loan(title, name, day))
    member.loans.append(title)
    log.append(f"day {day}: {name} took {title}")
    return True


def give_back(name, title, day):
    for loan in loans:
        if loan.member == name and loan.title == title and loan.returned < 0:
            loan.returned = day
            late = loan.days_late(day)
            member = members[name]
            member.loans.remove(title)
            if late > 0:
                fine = late * FINE_PER_DAY
                member.fines += fine
                log.append(f"day {day}: {name} returned {title}, {late} days late, fine {fine:.2f}")
            else:
                log.append(f"day {day}: {name} returned {title}")
            return True
    return False


def parse_event(line):
    """One line of the day book: `day name verb title`."""
    parts = line.split(" ", 3)
    if len(parts) < 4:
        raise ValueError("a short line: " + line)
    day = int(parts[0])
    return day, parts[1], parts[2], parts[3]


def by_decade():
    decades = {}
    for title in books:
        decade = books[title].year // 10 * 10
        if decade not in decades:
            decades[decade] = []
        decades[decade].append(title)
    return decades


def average_year():
    total = 0
    count = 0
    for title in books:
        total += books[title].year * books[title].copies
        count += books[title].copies
    return total / count


def spread_of_years():
    mean = average_year()
    squares = 0.0
    count = 0
    for title in books:
        squares += (books[title].year - mean) ** 2
        count += 1
    return math.sqrt(squares / count)


def longest(words):
    best = ""
    for word in words:
        if len(word) > len(best):
            best = word
    return best


def bar(count, width=20):
    filled = min(count, width)
    return "#" * filled + "." * (width - filled)


add_book("Dune", "Frank Herbert", 1965, 2)
add_book("Emma", "Jane Austen", 1815)
add_book("Kindred", "Octavia Butler", 1979)
add_book("Solaris", "Stanislaw Lem", 1961)
add_book("Dune", "Frank Herbert", 1965)
add_book("Beloved", "Toni Morrison", 1987, 2)

for name, joined in [("ann", 2019), ("bob", 2021), ("cy", 2023)]:
    add_member(name, joined)
try:
    add_member("ann", 2024)
except ValueError as error:
    print("refused:", error)

day_book = """1 ann borrow Dune
1 bob borrow Dune
2 cy borrow Dune
2 cy borrow Dune
3 ann borrow Emma
4 ann borrow Kindred
5 ann borrow Solaris
9 bob return Dune
20 cy borrow Emma
30 ann return Emma
31 cy borrow Emma
40 ann return Dune
41 ann sing Dune
oops
44 bob borrow Middlemarch
60 ann return Kindred
61 ann borrow Beloved"""

for line in day_book.splitlines():
    try:
        day, name, verb, title = parse_event(line)
    except ValueError as error:
        print("skipped:", error)
        continue
    if verb == "borrow":
        try:
            borrow(name, title, day)
        except KeyError:
            print("no such book:", title)
    elif verb == "return":
        if not give_back(name, title, day):
            print(name, "did not have", title)
    else:
        print("unknown verb:", verb)

print()
print("The day book")
for entry in log:
    print("  " + entry)

print()
print("Members")
for name in sorted(members):
    member = members[name]
    print(f"  {member.describe():<32} fines {member.fines:6.2f}")

print()
print("Shelves")
for title in sorted(books):
    book = books[title]
    left = available(title)
    print(f"  {title:<10} {book.author:<16} {book.year}  {bar(left, 4)} {left}/{book.copies}")

print()
decades = by_decade()
for decade in sorted(decades):
    print(f"  {decade}s: {', '.join(sorted(decades[decade]))}")
print(f"  mean year {average_year():.1f}, spread {spread_of_years():.2f}")
print("  longest title:", longest(list(books)))

still_out = [loan for loan in loans if loan.returned < 0]
late = [loan for loan in still_out if loan.days_late(70) > 0]
print()
print(len(loans), "loans,", len(still_out), "still out,", len(late), "late on day 70")
owed = sum(members[name].fines for name in members)
print(f"fines owed: {owed:.2f}")

print(json.dumps({"books": len(books), "members": len(members), "loans": len(loans)}, sort_keys=True))
if len(sys.argv) > 1:
    print("arguments:", sys.argv[1:])
