#include "wayland-wire.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include <math.h>

static int tests_run = 0;
static int tests_passed = 0;

#define TEST(name) \
    static void test_##name(void); \
    static void run_test_##name(void) { \
        tests_run++; \
        printf("Running test: %s...", #name); \
        test_##name(); \
        tests_passed++; \
        printf(" OK\n"); \
    } \
    static void test_##name(void)

TEST(int_uint_roundtrip) {
    wl_msg_builder_t b;
    wl_msg_builder_init(&b, 42, 1);
    assert(wl_msg_builder_add_int(&b, -123456) == 0);
    assert(wl_msg_builder_add_uint(&b, 0xdeadbeef) == 0);
    wl_msg_builder_finish(&b);

    wl_msg_reader_t r;
    assert(wl_msg_reader_init(&r, b.buffer, b.size) == 0);
    assert(r.header.object_id == 42);
    assert(r.header.opcode == 1);
    assert(r.header.size == 16);

    int32_t i;
    uint32_t u;
    assert(wl_msg_reader_get_int(&r, &i) == 0);
    assert(i == -123456);
    assert(wl_msg_reader_get_uint(&r, &u) == 0);
    assert(u == 0xdeadbeef);
}

TEST(fixed_roundtrip) {
    wl_msg_builder_t b;
    wl_msg_builder_init(&b, 10, 2);
    wl_fixed_t f1 = wl_fixed_from_int(42);
    wl_fixed_t f2 = wl_fixed_from_double(12.75);
    wl_fixed_t f3 = wl_fixed_from_double(-3.5);

    assert(wl_msg_builder_add_fixed(&b, f1) == 0);
    assert(wl_msg_builder_add_fixed(&b, f2) == 0);
    assert(wl_msg_builder_add_fixed(&b, f3) == 0);
    wl_msg_builder_finish(&b);

    wl_msg_reader_t r;
    assert(wl_msg_reader_init(&r, b.buffer, b.size) == 0);

    wl_fixed_t rf1, rf2, rf3;
    assert(wl_msg_reader_get_fixed(&r, &rf1) == 0);
    assert(wl_msg_reader_get_fixed(&r, &rf2) == 0);
    assert(wl_msg_reader_get_fixed(&r, &rf3) == 0);

    assert(wl_fixed_to_int(rf1) == 42);
    assert(fabs(wl_fixed_to_double(rf2) - 12.75) < 0.001);
    assert(fabs(wl_fixed_to_double(rf3) - (-3.5)) < 0.001);
}

TEST(string_roundtrip) {
    wl_msg_builder_t b;
    wl_msg_builder_init(&b, 1, 3);
    assert(wl_msg_builder_add_string(&b, "wl_compositor") == 0);
    assert(wl_msg_builder_add_string(&b, "test") == 0);
    assert(wl_msg_builder_add_string(&b, "") == 0);
    assert(wl_msg_builder_add_string(&b, NULL) == 0);
    wl_msg_builder_finish(&b);

    wl_msg_reader_t r;
    assert(wl_msg_reader_init(&r, b.buffer, b.size) == 0);

    const char *s1, *s2, *s3, *s4;
    uint32_t l1, l2, l3, l4;
    assert(wl_msg_reader_get_string(&r, &s1, &l1) == 0);
    assert(strcmp(s1, "wl_compositor") == 0);
    assert(l1 == 14);

    assert(wl_msg_reader_get_string(&r, &s2, &l2) == 0);
    assert(strcmp(s2, "test") == 0);
    assert(l2 == 5);

    assert(wl_msg_reader_get_string(&r, &s3, &l3) == 0);
    assert(strcmp(s3, "") == 0);
    assert(l3 == 1);

    assert(wl_msg_reader_get_string(&r, &s4, &l4) == 0);
    assert(s4 == NULL);
    assert(l4 == 0);
}

TEST(object_and_new_id) {
    wl_msg_builder_t b;
    wl_msg_builder_init(&b, 1, 0);
    assert(wl_msg_builder_add_object(&b, 100) == 0);
    assert(wl_msg_builder_add_new_id(&b, 101) == 0);
    wl_msg_builder_finish(&b);

    wl_msg_reader_t r;
    assert(wl_msg_reader_init(&r, b.buffer, b.size) == 0);

    uint32_t o, n;
    assert(wl_msg_reader_get_object(&r, &o) == 0);
    assert(o == 100);
    assert(wl_msg_reader_get_new_id(&r, &n) == 0);
    assert(n == 101);
}

TEST(new_id_typed) {
    wl_msg_builder_t b;
    wl_msg_builder_init(&b, 2, 0);
    assert(wl_msg_builder_add_new_id_typed(&b, "wl_shm", 1, 5) == 0);
    wl_msg_builder_finish(&b);

    wl_msg_reader_t r;
    assert(wl_msg_reader_init(&r, b.buffer, b.size) == 0);

    const char *iface;
    uint32_t ver, id;
    assert(wl_msg_reader_get_new_id_typed(&r, &iface, &ver, &id) == 0);
    assert(strcmp(iface, "wl_shm") == 0);
    assert(ver == 1);
    assert(id == 5);
}

TEST(array_roundtrip) {
    wl_msg_builder_t b;
    wl_msg_builder_init(&b, 3, 4);

    uint8_t payload[7] = { 1, 2, 3, 4, 5, 6, 7 };
    assert(wl_msg_builder_add_array(&b, payload, sizeof(payload)) == 0);
    assert(wl_msg_builder_add_array(&b, NULL, 0) == 0);
    wl_msg_builder_finish(&b);

    wl_msg_reader_t r;
    assert(wl_msg_reader_init(&r, b.buffer, b.size) == 0);

    const void *arr_data;
    uint32_t arr_len;
    assert(wl_msg_reader_get_array(&r, &arr_data, &arr_len) == 0);
    assert(arr_len == 7);
    assert(memcmp(arr_data, payload, 7) == 0);

    const void *arr_empty;
    uint32_t arr_empty_len;
    assert(wl_msg_reader_get_array(&r, &arr_empty, &arr_empty_len) == 0);
    assert(arr_empty_len == 0);
}

TEST(fd_passing) {
    wl_msg_builder_t b;
    wl_msg_builder_init(&b, 4, 5);
    assert(wl_msg_builder_add_fd(&b, 3) == 0);
    assert(wl_msg_builder_add_fd(&b, 7) == 0);
    assert(b.num_fds == 2);
    assert(b.fds[0] == 3);
    assert(b.fds[1] == 7);

    wl_connection_buffer_t buf;
    wl_conn_buffer_init(&buf);
    assert(wl_conn_buffer_put_fd(&buf, 3) == 0);
    assert(wl_conn_buffer_put_fd(&buf, 7) == 0);

    int fd1 = -1, fd2 = -1, fd3 = -1;
    assert(wl_conn_buffer_get_fd(&buf, &fd1) == 0 && fd1 == 3);
    assert(wl_conn_buffer_get_fd(&buf, &fd2) == 0 && fd2 == 7);
    assert(wl_conn_buffer_get_fd(&buf, &fd3) == -1);
}

TEST(header_validation) {
    wl_msg_builder_t b;
    wl_msg_builder_init(&b, 0x12345678, 0x0042);
    assert(wl_msg_builder_add_uint(&b, 100) == 0);
    wl_msg_builder_finish(&b);

    assert(b.size == 12);
    uint32_t obj_id;
    uint32_t word1;
    memcpy(&obj_id, b.buffer, 4);
    memcpy(&word1, b.buffer + 4, 4);

    assert(obj_id == 0x12345678);
    assert(((word1 >> 16) & 0xffff) == 12);
    assert((word1 & 0xffff) == 0x0042);
}

TEST(synthetic_registry_bind) {
    wl_msg_builder_t b;
    assert(wl_build_bind(&b, 2, 1, "wl_compositor", 4, 3) == 0);

    wl_msg_reader_t r;
    assert(wl_msg_reader_init(&r, b.buffer, b.size) == 0);
    assert(r.header.object_id == 2);
    assert(r.header.opcode == WL_REGISTRY_BIND);

    uint32_t name;
    assert(wl_msg_reader_get_uint(&r, &name) == 0);
    assert(name == 1);

    const char *iface;
    uint32_t ver, new_id;
    assert(wl_msg_reader_get_new_id_typed(&r, &iface, &ver, &new_id) == 0);
    assert(strcmp(iface, "wl_compositor") == 0);
    assert(ver == 4);
    assert(new_id == 3);
}

TEST(synthetic_global_event) {
    wl_msg_builder_t b;
    wl_msg_builder_init(&b, 2, WL_REGISTRY_GLOBAL);
    assert(wl_msg_builder_add_uint(&b, 5) == 0);
    assert(wl_msg_builder_add_string(&b, "wl_output") == 0);
    assert(wl_msg_builder_add_uint(&b, 2) == 0);
    wl_msg_builder_finish(&b);

    wl_msg_reader_t r;
    assert(wl_msg_reader_init(&r, b.buffer, b.size) == 0);
    assert(r.header.object_id == 2);
    assert(r.header.opcode == WL_REGISTRY_GLOBAL);

    wl_global_event_t global;
    assert(wl_decode_global_event(&r, &global) == 0);
    assert(global.name == 5);
    assert(strcmp(global.interface, "wl_output") == 0);
    assert(global.version == 2);
}

TEST(truncated_and_invalid_messages) {
    uint8_t short_data[6] = { 1, 0, 0, 0, 8, 0 };
    wl_msg_reader_t r;
    assert(wl_msg_reader_init(&r, short_data, sizeof(short_data)) == -1);

    /* Invalid header: size reported in header is 16, but buffer is only 12 */
    uint32_t bad_hdr[3] = { 1, (16 << 16) | 0, 100 };
    assert(wl_msg_reader_init(&r, (uint8_t *)bad_hdr, 12) == -1);

    /* Buffer read past end */
    uint32_t valid_hdr[2] = { 1, (8 << 16) | 0 };
    assert(wl_msg_reader_init(&r, (uint8_t *)valid_hdr, 8) == 0);
    int32_t val;
    assert(wl_msg_reader_get_int(&r, &val) == -1);
}

TEST(connection_buffer_streaming) {
    wl_connection_buffer_t buf;
    wl_conn_buffer_init(&buf);

    /* Construct 2 messages */
    wl_msg_builder_t b1, b2;
    assert(wl_build_get_registry(&b1, 1, 2) == 0);
    assert(wl_build_bind(&b2, 2, 1, "wl_shm", 1, 3) == 0);

    /* Put partial bytes of b1 */
    assert(wl_conn_buffer_put(&buf, b1.buffer, 4) == 0);
    const uint8_t *msg;
    size_t msg_len;
    assert(wl_conn_buffer_get_next_message(&buf, &msg, &msg_len) == 0);

    /* Put remaining of b1 and half of b2 */
    assert(wl_conn_buffer_put(&buf, b1.buffer + 4, b1.size - 4) == 0);
    assert(wl_conn_buffer_put(&buf, b2.buffer, 6) == 0);

    /* Now b1 is complete */
    assert(wl_conn_buffer_get_next_message(&buf, &msg, &msg_len) == 1);
    assert(msg_len == b1.size);
    assert(memcmp(msg, b1.buffer, b1.size) == 0);
    wl_conn_buffer_consume(&buf, msg_len);

    /* b2 is still partial */
    assert(wl_conn_buffer_get_next_message(&buf, &msg, &msg_len) == 0);

    /* Feed rest of b2 */
    assert(wl_conn_buffer_put(&buf, b2.buffer + 6, b2.size - 6) == 0);
    assert(wl_conn_buffer_get_next_message(&buf, &msg, &msg_len) == 1);
    assert(msg_len == b2.size);
    assert(memcmp(msg, b2.buffer, b2.size) == 0);
    wl_conn_buffer_consume(&buf, msg_len);

    /* Now empty */
    assert(wl_conn_buffer_get_next_message(&buf, &msg, &msg_len) == 0);
}

int main(void) {
    printf("Starting Wayland Wire Protocol Tests...\n");
    run_test_int_uint_roundtrip();
    run_test_fixed_roundtrip();
    run_test_string_roundtrip();
    run_test_object_and_new_id();
    run_test_new_id_typed();
    run_test_array_roundtrip();
    run_test_fd_passing();
    run_test_header_validation();
    run_test_synthetic_registry_bind();
    run_test_synthetic_global_event();
    run_test_truncated_and_invalid_messages();
    run_test_connection_buffer_streaming();

    printf("\nAll %d tests passed!\n", tests_passed);
    printf("WIRE TESTS PASSED\n");
    return 0;
}
