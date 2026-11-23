# kept: 0
# The comments of a file stay where they were.

# A comment on its own, with an empty line after it.

# The limit of the loop.
LIMIT = 3  # three times


def twice(n):
    # doubles
    result = n * 2  # the product
    # and gives it back
    return result


# Now the loop.
for i in range(LIMIT):
    # each time round
    if i == 1:  # the middle one
        print("middle")  # say so
    else:
        # the others
        print(twice(i))
# The end.
