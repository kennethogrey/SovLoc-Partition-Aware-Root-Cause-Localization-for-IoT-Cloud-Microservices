# One command reproduction for the SovLoc midterm. The only prerequisite is
# Docker with Compose v2. Everything runs inside a container, so there is no
# local Rust or Python to install and no "works on my machine".
#
#   make            build and run the full reproduction (same as ./run.sh)
#   make demo       bring up the real container mesh (production tier)
#   make partition  in a second terminal, cut the cloud collector for 40s
#   make netem      apply a real degraded link (250ms, 8% loss, 512kbit)
#   make clean      remove the reproduction image
.PHONY: all reproduce demo partition netem clean

all: reproduce

reproduce:
	./run.sh

demo:
	./scripts/compose_up.sh

partition:
	./scripts/partition_docker.sh 40

netem:
	./scripts/netem.sh cloud add 250 8 512

clean:
	-docker rmi sovloc-reproduce 2>/dev/null
	-docker compose down 2>/dev/null
