#include <stdint.h>
#include <stdlib.h>

/* Keep a small live set while exercising malloc/calloc/realloc/free. */
int main(void) {
    void *live[256] = {0};
    uint64_t iteration = 0;
    for (;;) {
        const size_t slot = iteration++ % 256;
        free(live[slot]);
        live[slot] = malloc(4096 + (iteration % 4096));
        if (live[slot] != NULL) {
            ((volatile unsigned char *)live[slot])[0] = (unsigned char)iteration;
        }
        if ((iteration & 127) == 0) {
            void *temporary = calloc(8, 128);
            temporary = realloc(temporary, 4096);
            free(temporary);
        }
    }
}
