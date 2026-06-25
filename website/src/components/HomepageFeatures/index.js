import clsx from 'clsx';
import Heading from '@theme/Heading';
import styles from './styles.module.css';

const FeatureList = [
  {
    title: 'Lean core',
    description: (
      <>
        DuckDB compiled to <code>wasm32-wasip2</code>. The core ships only
        <code> core_functions</code> + <code>parquet</code> (~44&nbsp;MB) — all
        other functionality is loadable rather than statically embedded.
      </>
    ),
  },
  {
    title: 'Extension components',
    description: (
      <>
        ~181 extensions are Rust <code>wasm32-wasip2</code> components
        implementing the <code>duckdb:extension</code> WIT world. Load one at
        runtime with <code>LOAD &lt;name&gt;</code> — no core recompile,
        version-independent.
      </>
    ),
  },
  {
    title: 'Composable & portable',
    description: (
      <>
        Components compose (one can plug another via <code>wac</code>), run
        unmodified across native, standalone, and in-browser hosts, and survive
        DuckDB version bumps behind a stable WIT contract.
      </>
    ),
  },
];

function Feature({title, description}) {
  return (
    <div className={clsx('col col--4')}>
      <div className="text--center padding-horiz--md">
        <Heading as="h3">{title}</Heading>
        <p>{description}</p>
      </div>
    </div>
  );
}

export default function HomepageFeatures() {
  return (
    <section className={styles.features}>
      <div className="container">
        <div className="row">
          {FeatureList.map((props, idx) => (
            <Feature key={idx} {...props} />
          ))}
        </div>
      </div>
    </section>
  );
}
