// kept: 3
/* What Zeno cannot say stays C, and is called from Zeno. */
#include <stdio.h>
#include <stdlib.h>

struct Node {
    int value;
    struct Node *next;
};

int next_id(void) {
    static int id = 0;
    return ++id;
}

int sum_list(int count) {
    struct Node *head = NULL;
    for (int i = 1; i <= count; i++) {
        struct Node *node = malloc(sizeof *node);
        node->value = i;
        node->next = head;
        head = node;
    }
    int total = 0;
    while (head != NULL) {
        struct Node *next = head->next;
        total += head->value;
        free(head);
        head = next;
    }
    return total;
}

int twice(int n) {
    return 2 * n;
}

int main(void) {
    int first = next_id();
    int second = next_id();
    printf("ids %d %d\n", first, second);
    printf("sum %d\n", sum_list(10));
    printf("twice %d\n", twice(21));
    return 0;
}
