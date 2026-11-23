// kept: 0
/* Texts that are read: letters, lengths, comparisons. */
#include <stdio.h>
#include <string.h>
#include <ctype.h>

int vowels(const char *text) {
    int count = 0;
    for (int i = 0; text[i] != '\0'; i++) {
        char c = tolower(text[i]);
        if (c == 'a' || c == 'e' || c == 'i' || c == 'o' || c == 'u') {
            count++;
        }
    }
    return count;
}

int digits_sum(const char *text) {
    int total = 0;
    int length = strlen(text);
    for (int i = 0; i < length; i++) {
        if (isdigit(text[i])) {
            total += text[i] - '0';
        }
    }
    return total;
}

const char *size_word(int n) {
    if (n < 10) {
        return "small";
    }
    return n < 100 ? "middling" : "large";
}

int main(void) {
    const char *words[] = {"Banana", "apple", "Cherry"};
    for (int i = 0; i < 3; i++) {
        printf("%-7s %d vowels, %zu letters\n", words[i], vowels(words[i]), strlen(words[i]));
    }
    printf("digits %d\n", digits_sum("a1b2c39"));
    if (strcmp(words[0], "Banana") == 0) {
        puts("the first is Banana");
    }
    if (strcmp(words[1], words[2]) > 0) {
        puts("apple comes after Cherry");
    }
    printf("%s %s %s\n", size_word(3), size_word(42), size_word(500));
    char shout = toupper('q');
    printf("%c%c\n", shout, toupper('!'));
    return 0;
}
