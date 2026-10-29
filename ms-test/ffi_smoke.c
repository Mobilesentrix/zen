// C ABI smoke test for libzen_ffi + database node. See MS-BUILD.md.
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "zen_engine.h"

// The static lib references the zen-go callbacks; a C program has to provide them.
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

static struct ZenCustomNodeResult other_kinds(const char *request) {
    (void)request;
    struct ZenCustomNodeResult r = {0};
    r.content = strdup("{\"output\":{\"fromC\":true},\"traceData\":null}");
    return r;
}

static const char *DECISIONS =
    "{\"db.json\":{\"nodes\":["
    "{\"id\":\"in\",\"name\":\"Request\",\"type\":\"inputNode\",\"position\":{\"x\":0,\"y\":0}},"
    "{\"id\":\"db\",\"name\":\"products\",\"type\":\"customNode\",\"position\":{\"x\":0,\"y\":0},"
    " \"content\":{\"kind\":\"database\",\"config\":{\"instance\":\"starrocks\","
    "  \"query\":\"SELECT sku, qty FROM products WHERE site = {{site ?? 'us'}} AND sku IN ({{skus}}) ORDER BY entity_id\","
    "  \"outputPath\":\"dbResult\",\"passThrough\":true}}},"
    "{\"id\":\"ex\",\"name\":\"sum\",\"type\":\"expressionNode\",\"position\":{\"x\":0,\"y\":0},"
    " \"content\":{\"expressions\":[{\"id\":\"x\",\"key\":\"totalQty\",\"value\":\"sum(map(dbResult.rows, #.qty))\"}],\"passThrough\":false}},"
    "{\"id\":\"c\",\"name\":\"c\",\"type\":\"customNode\",\"position\":{\"x\":0,\"y\":0},\"content\":{\"kind\":\"other\",\"config\":{}}},"
    "{\"id\":\"out\",\"name\":\"Response\",\"type\":\"outputNode\",\"position\":{\"x\":0,\"y\":0}}],"
    "\"edges\":["
    "{\"id\":\"e1\",\"type\":\"edge\",\"sourceId\":\"in\",\"targetId\":\"db\"},"
    "{\"id\":\"e2\",\"type\":\"edge\",\"sourceId\":\"db\",\"targetId\":\"ex\"},"
    "{\"id\":\"e3\",\"type\":\"edge\",\"sourceId\":\"ex\",\"targetId\":\"out\"},"
    "{\"id\":\"e4\",\"type\":\"edge\",\"sourceId\":\"in\",\"targetId\":\"c\"},"
    "{\"id\":\"e5\",\"type\":\"edge\",\"sourceId\":\"c\",\"targetId\":\"out\"}]},"
    // resultHandling: a single-row lookup that must find exactly one product
    "\"lookup.json\":{\"nodes\":["
    "{\"id\":\"in\",\"name\":\"Request\",\"type\":\"inputNode\",\"position\":{\"x\":0,\"y\":0}},"
    "{\"id\":\"db\",\"name\":\"Look up product\",\"type\":\"customNode\",\"position\":{\"x\":0,\"y\":0},"
    " \"content\":{\"kind\":\"database\",\"config\":{\"instance\":\"starrocks\","
    "  \"query\":\"SELECT sku, qty FROM products WHERE sku = {{sku}}\",\"passThrough\":false,"
    "  \"resultHandling\":{\"none\":\"error\",\"noneMessage\":\"No such product\",\"one\":\"use\",\"several\":\"error\"}}}},"
    "{\"id\":\"out\",\"name\":\"Response\",\"type\":\"outputNode\",\"position\":{\"x\":0,\"y\":0}}],"
    "\"edges\":["
    "{\"id\":\"e1\",\"type\":\"edge\",\"sourceId\":\"in\",\"targetId\":\"db\"},"
    "{\"id\":\"e2\",\"type\":\"edge\",\"sourceId\":\"db\",\"targetId\":\"out\"}]}}";

static const char *DB_CONFIG =
    "{\"connections\":{\"mysql-instance\":{\"url\":\"mysql://root:test@ms-zen-mysql:3306/ms_test\","
    "\"aliases\":[\"starrocks\"],\"connectEagerly\":true}}}";

static int failures = 0;
#define CHECK(cond, ...) do { if (!(cond)) { failures++; printf("FAIL: " __VA_ARGS__); printf("\n"); } else { printf("ok: " __VA_ARGS__); printf("\n"); } } while (0)

int main(void) {
    struct ZenEngineLoaderConfig cfg = { ZenLoaderConfigKind_Static, DECISIONS, NULL, 0 };

    // 1. invalid config -> error 12 with details
    struct ZenResult_ZenEngineStruct bad =
        zen_engine_new_with_loader_config_and_database(cfg, NULL, "{\"connections\":{\"x\":{\"url\":\"oracle://h\"}}}");
    CHECK(bad.error == 12 && bad.result == NULL && bad.details && strstr(bad.details, "unsupported database scheme"),
          "bad config -> code %d details %s", bad.error, bad.details ? bad.details : "(null)");
    free(bad.details);

    struct ZenResult_ZenEngineStruct nul = zen_engine_new_with_loader_config_and_database(cfg, NULL, NULL);
    CHECK(nul.error == 12, "null config -> code %d", nul.error);
    free(nul.details);

    // 2. real engine
    struct ZenResult_ZenEngineStruct e = zen_engine_new_with_loader_config_and_database(cfg, other_kinds, DB_CONFIG);
    CHECK(e.error == 0 && e.result != NULL, "engine created (code %d %s)", e.error, e.details ? e.details : "");
    if (e.error != 0) return 1;

    struct ZenEngineEvaluationOptions opts = { false, 5 };
    for (int i = 0; i < 3; i++) {
        struct ZenResult_c_char r = zen_engine_evaluate(e.result, "db.json",
            "{\"skus\":[\"SKU-A\",\"SKU-B\",\"x' or '1'='1\"]}", opts);
        CHECK(r.error == 0 && r.result && strstr(r.result, "\"totalQty\":13") && strstr(r.result, "\"fromC\":true"),
              "evaluate #%d -> code %d %s", i, r.error, r.result ? r.result : (r.details ? r.details : ""));
        free(r.result);
        free(r.details);
    }

    // 2b. batch evaluation runs on upstream's pool of thread-local runtimes
    struct ZenEngineEvaluateBatchRequest reqs[64];
    for (int i = 0; i < 64; i++) {
        reqs[i].key = "db.json";
        reqs[i].context = (i % 2) ? "{\"skus\":[\"SKU-A\"]}" : "{\"skus\":[\"SKU-B\"],\"site\":\"us\"}";
    }
    struct ZenResult_c_char b = zen_engine_evaluate_batch(e.result, reqs, 64, opts);
    int a_hits = 0, b_hits = 0;
    for (const char *p = b.result; p && (p = strstr(p, "\"totalQty\":10")); p++) a_hits++;
    for (const char *p = b.result; p && (p = strstr(p, "\"totalQty\":3")); p++) b_hits++;
    CHECK(b.error == 0 && a_hits == 32 && b_hits == 32, "batch of 64 -> code %d, %d x 10, %d x 3", b.error, a_hits, b_hits);
    free(b.result);
    free(b.details);

    // 3. evaluation error (null placeholder) -> code 7 with node error
    struct ZenResult_c_char r = zen_engine_evaluate(e.result, "db.json", "{\"skus\":null}", opts);
    CHECK(r.error == 7 && r.details && strstr(r.details, "resolved to null"),
          "null placeholder -> code %d %s", r.error, r.details ? r.details : "");
    free(r.result);
    free(r.details);

    // 3a. traced, the failing step's trace entry says what failed (MS-FFI.md "Error trace");
    //     untraced (3. above) has no trace
    struct ZenEngineEvaluationOptions traced = { true, 5 };
    struct ZenResult_c_char t = zen_engine_evaluate(e.result, "db.json", "{\"skus\":null}", traced);
    CHECK(t.error == 7 && t.details && strstr(t.details, "\"source\":\"Database query placeholder")
          && strstr(t.details, "\"kind\":\"missingValue\"") && strstr(t.details, "\"part\":\"placeholder\"")
          && strstr(t.details, "\"expression\":\"skus\"") && strstr(t.details, "\"missing\":[\"skus\"]"),
          "traced null placeholder -> code %d %s", t.error, t.details ? t.details : "");
    free(t.result);
    free(t.details);
    struct ZenResult_c_char u = zen_engine_evaluate(e.result, "db.json", "{\"skus\":null}", opts);
    CHECK(u.error == 7 && u.details && !strstr(u.details, "\"kind\"") && !strstr(u.details, "\"trace\""),
          "untraced error has no trace -> %s", u.details ? u.details : "");
    free(u.result);
    free(u.details);

    // 3b. resultHandling (MS-FFI.md §3): one row -> object; none -> node error with details
    struct ZenResult_c_char one = zen_engine_evaluate(e.result, "lookup.json", "{\"sku\":\"SKU-B\"}", opts);
    CHECK(one.error == 0 && one.result && strstr(one.result, "\"sku\":\"SKU-B\"") && strstr(one.result, "\"qty\":3")
          && strstr(one.result, "\"rowsCount\":1") && !strstr(one.result, "\"rows\":"),
          "resultHandling one -> code %d %s", one.error, one.result ? one.result : (one.details ? one.details : ""));
    free(one.result);
    free(one.details);
    struct ZenResult_c_char none = zen_engine_evaluate(e.result, "lookup.json", "{\"sku\":\"NOPE\"}", opts);
    CHECK(none.error == 7 && none.details && strstr(none.details, "\"source\":\"No such product\"")
          && strstr(none.details, "\"details\":{\"reason\":\"none\",\"rowsCount\":0}"),
          "resultHandling none -> code %d %s", none.error, none.details ? none.details : "");
    free(none.result);
    free(none.details);

    // 4. decision cache control (MS-FFI.md §1): loader-config engine recompiles, args validated
    CHECK(zen_engine_invalidate_decision(e.result, "db.json") == 0, "invalidate -> 0");
    CHECK(zen_engine_clear_decisions(e.result) == 0, "clear -> 0");
    CHECK(zen_engine_invalidate_decision(NULL, "db.json") == 1 && zen_engine_invalidate_decision(e.result, NULL) == 1
          && zen_engine_clear_decisions(NULL) == 1, "null args -> 1");
    struct ZenResult_c_char after = zen_engine_evaluate(e.result, "db.json", "{\"skus\":[\"SKU-A\"]}", opts);
    CHECK(after.error == 0 && after.result && strstr(after.result, "\"totalQty\":10"),
          "evaluate after cache clear -> code %d", after.error);
    free(after.result);
    free(after.details);

    zen_engine_free(e.result);
    printf("%s (%d failures)\n", failures ? "FAILED" : "PASSED", failures);
    return failures ? 1 : 0;
}
