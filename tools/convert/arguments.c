// kept: 2
/* A main that reads the words it is given. */
#include <stdio.h>

int count_letters(const char *word) {
    int count = 0;
    while (word[count] != '\0') {
        count++;
    }
    return count;
}

int main(int argc, char *argv[]) {
    printf("%d words\n", argc - 1);
    for (int i = 1; i < argc; i++) {
        printf("%s has %d letters\n", argv[i], count_letters(argv[i]));
    }
    if (argc > 3) {
        fprintf(stderr, "usage: %s [word] [word]\n", argv[0]);
        return 2;
    }
    return 0;
}
