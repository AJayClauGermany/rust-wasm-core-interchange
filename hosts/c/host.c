/*
 * A host in C: loads a WIT module, gives it a host command and calls it.
 *
 *   cc host.c -I ../../crates/wasm-backend-ffi/include -L <dir of libwasm_backend> \
 *      -lwasm_backend -o host
 *   ./host component_guest.wasm
 *
 * The module is guests/rust/component-guest.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "wasm_backend.h"

/* Stop with the message a failed call left in `out`. */
static void check(WasmBackendStatus status, char *out, const char *what) {
  if (status == WASM_BACKEND_STATUS_OK) return;
  fprintf(stderr, "%s failed (%d): %s\n", what, (int)status, out ? out : "");
  exit(1);
}

/* Events modules emit, and the backend's own (module-loaded, ...). */
static void on_event(void *user_data, const char *event, const char *payload_json) {
  (void)user_data;
  if (strncmp(event, "wasm-backend://", 15) == 0)
    printf("event %s\n", event);
  else
    printf("event %s %s\n", event, payload_json);
}

/* The host command `app_name`: what modules get when they ask for it. */
static void app_name(void *user_data, const char *module, const char *args_json,
                     WasmBackendReply *reply) {
  (void)args_json;
  printf("%s asked for the app's name\n", module);
  wasm_backend_reply_ok(reply, (const char *)user_data);
}

static unsigned char *read_file(const char *path, size_t *len) {
  FILE *file = fopen(path, "rb");
  if (!file) {
    perror(path);
    exit(1);
  }
  fseek(file, 0, SEEK_END);
  *len = (size_t)ftell(file);
  rewind(file);
  unsigned char *bytes = malloc(*len);
  if (fread(bytes, 1, *len, file) != *len) {
    perror(path);
    exit(1);
  }
  fclose(file);
  return bytes;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s <component_guest.wasm>\n", argv[0]);
    return 2;
  }
  char *out = NULL;

  /* Limits for every module, then the callbacks, then the backend. */
  WasmBackendBuilder *builder = NULL;
  check(wasm_backend_builder_new("{\"limits\": {\"timeoutMs\": 5000, \"memoryLimitMb\": 64}}",
                                 &builder, &out),
        out, "configuring");
  wasm_backend_builder_on_event(builder, on_event, NULL);
  check(wasm_backend_builder_host_command(builder, "app_name", app_name, "\"C host\"", &out), out,
        "registering app_name");
  WasmBackend *backend = NULL;
  check(wasm_backend_builder_build(builder, &backend, &out), out, "building");

  /* A module gets nothing it isn't granted here. */
  size_t len = 0;
  unsigned char *bytes = read_file(argv[1], &len);
  check(wasm_backend_load_bytes(
            backend, "fixture", bytes, len,
            "{\"capabilities\": {\"hostCommands\": [\"app_name\"],"
            " \"events\": {\"emit\": [\"test://*\"]}}}",
            &out),
        out, "loading");
  wasm_backend_string_free(out);
  free(bytes);

  check(wasm_backend_invoke(backend, "fixture", "greet", "{\"name\": \"C\"}", &out), out, "greet");
  printf("greet -> %s\n", out);
  wasm_backend_string_free(out);

  check(wasm_backend_invoke(backend, "fixture", "call-host",
                            "{\"command\": \"app_name\", \"args\": \"null\"}", &out),
        out, "call-host");
  printf("call-host -> %s\n", out);
  wasm_backend_string_free(out);

  check(wasm_backend_invoke(backend, "fixture", "emit-event",
                            "{\"name\": \"test://hello\", \"payload\": \"[1, 2]\"}", &out),
        out, "emit-event");
  wasm_backend_string_free(out);

  /* An error the module returns is a status of its own, with the value. */
  WasmBackendStatus status = wasm_backend_invoke(
      backend, "fixture", "describe", "{\"person\": {\"name\": \"Old\", \"ageYears\": 200}}", &out);
  if (status == WASM_BACKEND_STATUS_COMMAND) printf("describe -> error %s\n", out);
  wasm_backend_string_free(out);

  /* What it was not granted is refused. */
  status = wasm_backend_invoke(backend, "fixture", "emit-event",
                               "{\"name\": \"other://x\", \"payload\": \"1\"}", &out);
  printf("emitting other://x -> %s\n", status == WASM_BACKEND_STATUS_OK ? "allowed" : "refused");
  wasm_backend_string_free(out);

  wasm_backend_free(backend);
  return 0;
}
