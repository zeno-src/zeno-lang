// kept: 0
/* Structs: values that C copies, and pointers to them. */
#include <stdio.h>

struct Point {
    int x;
    int y;
};

typedef struct {
    char *name;
    int stock;
    double price;
} Item;

void move(struct Point *p, int dx, int dy) {
    p->x += dx;
    p->y += dy;
}

struct Point moved(struct Point p, int dx) {
    p.x += dx;
    return p;
}

int manhattan(struct Point a, struct Point b) {
    int dx = a.x - b.x;
    int dy = a.y - b.y;
    if (dx < 0) dx = -dx;
    if (dy < 0) dy = -dy;
    return dx + dy;
}

double worth(Item items[], int count) {
    double total = 0.0;
    for (int i = 0; i < count; i++) {
        total += items[i].stock * items[i].price;
    }
    return total;
}

int main(void) {
    struct Point a = {1, 2};
    struct Point b = a;
    b.x = 10;
    move(&a, 3, 4);
    struct Point c = moved(a, 100);
    printf("a=(%d,%d) b=(%d,%d) c=(%d,%d)\n", a.x, a.y, b.x, b.y, c.x, c.y);
    printf("distance %d\n", manhattan(a, b));
    Item shop[3] = {{"tea", 4, 2.5}, {"jam", 2, 3.25}, {"cake", 1, 7.0}};
    shop[1].stock += 3;
    for (int i = 0; i < 3; i++) {
        printf("%-5s %2d %6.2f\n", shop[i].name, shop[i].stock, shop[i].price);
    }
    printf("worth %.2f\n", worth(shop, 3));
    struct Point grid[2] = {{0, 0}, {5, 5}};
    move(&grid[1], -1, -1);
    printf("grid[1]=(%d,%d)\n", grid[1].x, grid[1].y);
    return 0;
}
