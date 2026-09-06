/* Reports whether DynamoRIO's standalone decoder understands an instruction.
 *
 * Reads one hexadecimal instruction word per line on stdin and writes
 * "<word>\tOK\t<disassembly>" or "<word>\tFAIL" per line. The driver in
 * report.py turns objdump output into that input and groups the failures.
 *
 * Instruction words are decoded out of their original address, so PC-relative
 * operands disassemble against a synthetic base. Only decodability matters
 * here.
 */
#include "dr_api.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int
main(void)
{
    char line[64];
    void *context = GLOBAL_DCONTEXT;

    while (fgets(line, sizeof(line), stdin) != NULL) {
        unsigned long word = strtoul(line, NULL, 16);
        byte buffer[8] = { 0 };
        instr_t *instr;
        byte *next;

        if (word == 0 && line[0] != '0')
            continue;
        for (size_t i = 0; i < sizeof(buffer); i++)
            buffer[i] = (byte)(word >> (8 * i));
        instr = instr_create(context);
        next = decode(context, buffer, instr);
        if (next == NULL) {
            printf("%lx\tFAIL\n", word);
        } else {
            char text[192];
            instr_disassemble_to_buffer(context, instr, text, sizeof(text));
            printf("%lx\tOK\t%s\n", word, text);
        }
        instr_destroy(context, instr);
    }
    return 0;
}
