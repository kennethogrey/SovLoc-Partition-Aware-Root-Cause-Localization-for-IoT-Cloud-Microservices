# One-command reproduction for SovLoc.
#
# Native pipeline (needs the pinned Rust toolchain and a TeX install):
#   make reproduce     build, run every experiment, analyse, and plot
#   make sim           run the experiment driver (writes results/raw_*.csv)
#   make analysis      compute tables, survival stats, numbers.tex, plot data
#   make figures       compile the results figure from the plot data
#   make paper         compile the manuscript (paper/overleaf/access.tex)
#   make test          run the unit tests
#   make clean         remove build artifacts
#
# Container demo (needs Docker with Compose v2):
#   make demo / demo-live / inject SERVICE=x / clear / partition / netem
#   make reproduce-docker   the original fully containerised reproduction
.PHONY: all reproduce sim analysis figures paper test clean \
        reproduce-docker demo demo-live inject clear partition netem docker-clean

all: reproduce

reproduce: sim analysis figures
	@echo ""
	@echo "Reproduction complete."
	@echo "  results/raw_*.csv            raw per-run data"
	@echo "  results/analysis/            summary tables, stats.txt, numbers.tex"
	@echo "  paper/overleaf/fig_sovloc_results.pdf   results figure"
	@echo "Run 'make paper' to compile the manuscript."

sim:
	cargo build --release --bin sim
	./target/release/sim --out-dir results

analysis:
	cargo build --release --bin analysis
	./target/release/analysis results
	@mkdir -p paper/overleaf
	@cp results/analysis/numbers.tex paper/overleaf/numbers.tex
	@echo "Copied numbers.tex into the manuscript project."

figures:
	cd paper/figures && pdflatex -interaction=nonstopmode -halt-on-error sovloc_results.tex >/dev/null 2>&1 \
	  && cp sovloc_results.pdf ../overleaf/fig_sovloc_results.pdf \
	  && rm -f sovloc_results.aux sovloc_results.log
	@echo "Built paper/overleaf/fig_sovloc_results.pdf"

paper:
	cd paper/overleaf && latexmk -pdf -interaction=nonstopmode -halt-on-error access.tex >/dev/null 2>&1 \
	  && echo "Built paper/overleaf/access.pdf" || echo "LaTeX build had warnings; see paper/overleaf/access.log"

test:
	cargo test

clean:
	cargo clean
	rm -f paper/figures/*.aux paper/figures/*.log
	rm -f paper/overleaf/*.aux paper/overleaf/*.log paper/overleaf/*.out paper/overleaf/*.fls paper/overleaf/*.fdb_latexmk

# ---- container demo (unchanged from the midterm package) --------------------
reproduce-docker:
	./run.sh
demo:
	./scripts/compose_up.sh
demo-live:
	./scripts/compose_up.sh none
inject:
	./scripts/inject.sh $(SERVICE)
clear:
	./scripts/inject.sh none
partition:
	./scripts/partition_docker.sh 40
netem:
	./scripts/netem.sh cloud add 250 8 512
docker-clean:
	-docker rmi sovloc-reproduce 2>/dev/null
	-docker compose down 2>/dev/null
