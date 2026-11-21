#include <stddef.h>
#include <stdio.h>

struct Inner {
    char tag;
    int value;
};

struct Layout {
    char tag;
    struct Inner items[3];
    void *owner;
    short tail;
};

volatile struct Layout layout;

int main(void) {
    printf("{\"size\":%zu,\"alignment\":%zu,\"fields\":["
           "{\"name\":\"tag\",\"offset\":%zu,\"size\":%zu},"
           "{\"name\":\"items\",\"offset\":%zu,\"size\":%zu},"
           "{\"name\":\"owner\",\"offset\":%zu,\"size\":%zu},"
           "{\"name\":\"tail\",\"offset\":%zu,\"size\":%zu}]}\n",
           sizeof(struct Layout), _Alignof(struct Layout),
           offsetof(struct Layout, tag), sizeof(layout.tag),
           offsetof(struct Layout, items), sizeof(layout.items),
           offsetof(struct Layout, owner), sizeof(layout.owner),
           offsetof(struct Layout, tail), sizeof(layout.tail));
    return 0;
}
