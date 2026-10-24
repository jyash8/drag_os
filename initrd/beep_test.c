#include <stdio.h>
#include <unistd.h>
#include <fcntl.h>

long syscall(long n, ...);

int main() {
    printf("Playing Drag OS PC Speaker Audio Test...\n");

    // Frequencies for C4, D4, E4, F4, G4, A4, B4, C5
    int notes[] = {262, 294, 330, 349, 392, 440, 494, 523};
    int num_notes = sizeof(notes) / sizeof(notes[0]);

    for (int i = 0; i < num_notes; i++) {
        printf("Playing note %d: %d Hz\n", i + 1, notes[i]);
        syscall(334, notes[i], 60);  // Short 60ms pulse for pleasant tone
        usleep(150000);              // Wait 150ms
    }

    printf("Audio Test Completed Successfully!\n");
    return 0;
}
