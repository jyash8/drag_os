#ifndef WAYLAND_WIRE_H
#define WAYLAND_WIRE_H

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Fixed point (24.8) type and helpers */
typedef int32_t wl_fixed_t;

static inline wl_fixed_t wl_fixed_from_int(int i) {
    return i * 256;
}

static inline int wl_fixed_to_int(wl_fixed_t f) {
    return f / 256;
}

static inline wl_fixed_t wl_fixed_from_double(double d) {
    return (wl_fixed_t)(d * 256.0);
}

static inline double wl_fixed_to_double(wl_fixed_t f) {
    return ((double)f) / 256.0;
}

/* Array type */
typedef struct {
    size_t size;
    size_t alloc;
    void *data;
} wl_array_t;

void wl_array_init(wl_array_t *array);
void wl_array_release(wl_array_t *array);
int wl_array_add(wl_array_t *array, const void *data, size_t size);

/* Message Header: 8 bytes */
typedef struct {
    uint32_t object_id;
    uint16_t opcode;
    uint16_t size;      /* in bytes, including 8-byte header */
} wl_msg_header_t;

/* Connection Buffer for accumulating and parsing */
#define WL_BUFFER_CAPACITY 65536

typedef struct {
    uint8_t data[WL_BUFFER_CAPACITY];
    size_t head; /* total bytes buffered */
    size_t tail; /* read position */
    int fds[16];
    size_t fd_head;
    size_t fd_tail;
} wl_connection_buffer_t;

void wl_conn_buffer_init(wl_connection_buffer_t *buf);
int wl_conn_buffer_put(wl_connection_buffer_t *buf, const void *data, size_t len);
int wl_conn_buffer_put_fd(wl_connection_buffer_t *buf, int fd);
int wl_conn_buffer_get_fd(wl_connection_buffer_t *buf, int *out_fd);
void wl_conn_buffer_consume(wl_connection_buffer_t *buf, size_t len);
int wl_conn_buffer_get_next_message(wl_connection_buffer_t *buf, const uint8_t **out_msg, size_t *out_size);

/* Message Builder / Serializer */
typedef struct {
    uint8_t buffer[WL_BUFFER_CAPACITY];
    size_t size;
    int fds[16];
    size_t num_fds;
} wl_msg_builder_t;

void wl_msg_builder_init(wl_msg_builder_t *b, uint32_t object_id, uint16_t opcode);
int wl_msg_builder_add_int(wl_msg_builder_t *b, int32_t i);
int wl_msg_builder_add_uint(wl_msg_builder_t *b, uint32_t u);
int wl_msg_builder_add_fixed(wl_msg_builder_t *b, wl_fixed_t f);
int wl_msg_builder_add_string(wl_msg_builder_t *b, const char *s);
int wl_msg_builder_add_object(wl_msg_builder_t *b, uint32_t o);
int wl_msg_builder_add_new_id(wl_msg_builder_t *b, uint32_t new_id);
int wl_msg_builder_add_new_id_typed(wl_msg_builder_t *b, const char *interface, uint32_t version, uint32_t new_id);
int wl_msg_builder_add_array(wl_msg_builder_t *b, const void *data, uint32_t len);
int wl_msg_builder_add_fd(wl_msg_builder_t *b, int fd);
void wl_msg_builder_finish(wl_msg_builder_t *b);

/* Message Parser / Reader */
typedef struct {
    const uint8_t *data;
    size_t size;
    size_t offset;
    wl_msg_header_t header;
} wl_msg_reader_t;

int wl_msg_reader_init(wl_msg_reader_t *r, const uint8_t *data, size_t size);
int wl_msg_reader_get_int(wl_msg_reader_t *r, int32_t *out_i);
int wl_msg_reader_get_uint(wl_msg_reader_t *r, uint32_t *out_u);
int wl_msg_reader_get_fixed(wl_msg_reader_t *r, wl_fixed_t *out_f);
int wl_msg_reader_get_string(wl_msg_reader_t *r, const char **out_s, uint32_t *out_len);
int wl_msg_reader_get_object(wl_msg_reader_t *r, uint32_t *out_o);
int wl_msg_reader_get_new_id(wl_msg_reader_t *r, uint32_t *out_new_id);
int wl_msg_reader_get_new_id_typed(wl_msg_reader_t *r, const char **out_interface, uint32_t *out_version, uint32_t *out_new_id);
int wl_msg_reader_get_array(wl_msg_reader_t *r, const void **out_data, uint32_t *out_len);

/* High-level Wayland Display / Registry Helpers */
#define WL_DISPLAY_ID 1

#define WL_DISPLAY_SYNC 0
#define WL_DISPLAY_GET_REGISTRY 1
#define WL_DISPLAY_ERROR 0
#define WL_DISPLAY_DELETE_ID 1

#define WL_REGISTRY_BIND 0
#define WL_REGISTRY_GLOBAL 0
#define WL_REGISTRY_GLOBAL_REMOVE 1

int wl_build_get_registry(wl_msg_builder_t *b, uint32_t display_id, uint32_t registry_id);
int wl_build_bind(wl_msg_builder_t *b, uint32_t registry_id, uint32_t name, const char *interface, uint32_t version, uint32_t new_id);

typedef struct {
    uint32_t name;
    const char *interface;
    uint32_t version;
} wl_global_event_t;

int wl_decode_global_event(wl_msg_reader_t *r, wl_global_event_t *out_global);

#ifdef __cplusplus
}
#endif

#endif /* WAYLAND_WIRE_H */
