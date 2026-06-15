#include "../../include/ctx_engine.h"

#include <stdio.h>
#include <stdlib.h>

static int call_and_print(CtxEngine *engine, const char *request) {
    char *response = ctx_engine_handle_request(engine, request);
    if (response == NULL) {
        fprintf(stderr, "ctx_engine_handle_request returned null\n");
        return 1;
    }
    puts(response);
    ctx_engine_free_string(response);
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <root>\n", argv[0]);
        return 2;
    }

    CtxEngine *engine = ctx_engine_new(argv[1]);
    if (engine == NULL) {
        fprintf(stderr, "ctx_engine_new returned null\n");
        return 1;
    }

    int status = 0;
    status |= call_and_print(engine,
        "{\"name\":\"file_search\",\"arguments\":{\"pattern\":\"pub\",\"mode\":\"content\",\"max_results\":2,\"context_lines\":0}}"
    );
    status |= call_and_print(engine,
        "{\"name\":\"read_file\",\"arguments\":{\"path\":\"ctx-core/src/lib.rs\",\"start_line\":1,\"limit\":3}}"
    );

    ctx_engine_free(engine);
    return status;
}
