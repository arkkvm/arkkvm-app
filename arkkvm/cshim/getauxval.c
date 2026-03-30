#include <stddef.h>
#include <elf.h>
extern char **environ;

unsigned long getauxval(unsigned long type)
{
    char **env = environ;
    while (*env != NULL) {
        env++;
    }

    unsigned long *auxv = (unsigned long *)(env + 1);

    while (*auxv != 0) {
        if (*auxv == type) {
            return *(auxv + 1);
        }
        auxv += 2;
    }

    return 0;
}
