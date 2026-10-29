// A Database step comparing a DECIMAL column with a request value, through libzen_ffi.a,
// against a StarRocks configured with the mysql:// scheme on port 9030 (testdb).
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "zen_engine.h"

struct ZenDecisionLoaderResult zen_engine_go_loader_callback(uintptr_t h, const char *key) {
    (void)h; (void)key;
    struct ZenDecisionLoaderResult r = {0};
    return r;
}
struct ZenCustomNodeResult zen_engine_go_custom_node_callback(uintptr_t h, const char *req) {
    (void)h; (void)req;
    struct ZenCustomNodeResult r = {0};
    return r;
}

static const char *DECISIONS =
    "{\"decimal.json\":{\"nodes\":["
    "{\"id\":\"in\",\"name\":\"Request\",\"type\":\"inputNode\",\"position\":{\"x\":0,\"y\":0}},"
    "{\"id\":\"db\",\"name\":\"Database\",\"type\":\"customNode\",\"position\":{\"x\":0,\"y\":0},"
    " \"content\":{\"kind\":\"database\",\"config\":{"
    "  \"query\":\"SELECT t0.sku FROM product_catalog.products t0 WHERE t0.cost = {{ cost }}\","
    "  \"passThrough\":false}}},"
    "{\"id\":\"out\",\"name\":\"Response\",\"type\":\"outputNode\",\"position\":{\"x\":0,\"y\":0}}],"
    "\"edges\":["
    "{\"id\":\"e1\",\"type\":\"edge\",\"sourceId\":\"in\",\"targetId\":\"db\"},"
    "{\"id\":\"e2\",\"type\":\"edge\",\"sourceId\":\"db\",\"targetId\":\"out\"}]}}";

static const char *DB_CONFIG =
    "{\"connections\":{\"ddd3956b\":{\"url\":\"mysql://root@starrocks:9030/mobilesentrix_dev\","
    "\"aliases\":[\"starrocks\"]}}}";

static int failures = 0;
#define CHECK(cond, ...) do { if (!(cond)) { failures++; printf("FAIL: " __VA_ARGS__); printf("\n"); } else { printf("ok: " __VA_ARGS__); printf("\n"); } } while (0)

static void run(struct ZenEngineStruct *e, const char *input, const char *expect) {
    struct ZenEngineEvaluationOptions opts = { false, 5 };
    struct ZenResult_c_char r = zen_engine_evaluate(e, "decimal.json", input, opts);
    CHECK(r.error == 0 && r.result && strstr(r.result, expect),
          "%s -> code %d %s", input, r.error, r.result ? r.result : (r.details ? r.details : ""));
    free(r.result);
    free(r.details);
}

int main(void) {
    struct ZenEngineLoaderConfig cfg = { ZenLoaderConfigKind_Static, DECISIONS, NULL, 0 };
    struct ZenResult_ZenEngineStruct e = zen_engine_new_with_loader_config_and_database(cfg, NULL, DB_CONFIG);
    CHECK(e.error == 0 && e.result != NULL, "engine created (code %d %s)", e.error, e.details ? e.details : "");
    if (e.error != 0) return 1;

    run(e.result, "{\"cost\":9.62}", "\"rows\":[{\"sku\":\"107000001022\"}]");
    run(e.result, "{\"cost\":26.12}", "\"rows\":[{\"sku\":\"107082074715\"}]");
    run(e.result, "{\"cost\":9.6201}", "\"rowsCount\":0");
    run(e.result, "{\"cost\":20}", "\"rowsCount\":0");

    zen_engine_free(e.result);
    printf(failures ? "FAILED (%d)\n" : "PASSED (0 failures)\n", failures);
    return failures ? 1 : 0;
}
