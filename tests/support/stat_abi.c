#include <sys/stat.h>
#include <stdio.h>

int main(void) {
    return printf("%zu\n", sizeof(struct stat)) < 0;
}
