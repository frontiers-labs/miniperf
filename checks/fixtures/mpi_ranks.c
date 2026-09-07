// Minimal two-rank MPI program: the shim interposes MPI_Init and MPI_Finalize,
// which is where it exchanges clocks between peers.
#include <mpi.h>
#include <stdio.h>

int main(int argc, char **argv) {
  MPI_Init(&argc, &argv);
  int rank = 0, size = 0;
  MPI_Comm_rank(MPI_COMM_WORLD, &rank);
  MPI_Comm_size(MPI_COMM_WORLD, &size);
  printf("rank %d of %d\n", rank, size);
  MPI_Finalize();
  return 0;
}
