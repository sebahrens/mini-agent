# Operational-risk dataset survey

Research date: 2026-10-13

Scope: operational risk management in banking, insurance, and reinsurance. This inventory deliberately separates actual operational-loss/event data from adjacent control datasets (fraud, AML, complaints, enforcement, cyber incidents, and contract-processing data). The latter are useful for detection, KRIs, scenarios, and control testing, but should not be treated as representative operational-loss samples.

## Main finding

Public, granular, institution-level operational-loss data is scarce. The strongest standardized banking and insurance loss collections are commercial or consortium products (ORX, GOLD, IBM FIRST, and SAS OpRisk Global Data). Public sources are mostly:

1. regulatory aggregates rather than event-level losses;
2. publicly reported cyber, enforcement, and complaint events;
3. synthetic fraud/AML datasets;
4. insurance claims or catastrophe data, which usually measure underwriting risk rather than the insurer's own operational risk.

No credible open, event-level reinsurance operational-loss dataset was found. The downloaded reinsurance corpus is a document-classification/control dataset, not loss data.

## Downloaded files

All paths are relative to this directory. Integrity hashes are in `SHA256SUMS`. ZIP/XLSX archives were tested successfully; the PDF and Parquet signatures were checked. Total download size is about 755 MiB.

| Sector/use | Local file | Size | Contents and suitability | License/access caveat | Source |
|---|---:|---:|---|---|---|
| Banking; direct-ish op-risk events | `downloads/kaggle/operational-risk-events-dataset.zip` | 40 KiB | 2,147 rows: 858 uploader-labelled OLE records and 1,289 ALE Monte Carlo records; event type, process, loss, frequency, severity, OpVaR, macro factors, class. Good for prototyping only. No source trail substantiates that OLE rows are real observations. | CC0 per Kaggle | https://www.kaggle.com/datasets/ziya07/operational-risk-events-dataset |
| Banking; anomaly/KRI practice | `downloads/kaggle/bank-transaction-fraud-detection.zip` | 102 KiB | 2,512 transactions with amount, channel, device, IP, login attempts, duration, balance, and customer attributes. It has no fraud target, so this is an anomaly-detection/control-testing dataset, not labeled losses. | Apache-2.0 per Kaggle | https://www.kaggle.com/datasets/valakhorasani/bank-transaction-dataset-for-fraud-detection |
| Banking; AML/fraud | `downloads/kaggle/ibm-amlsim-example-dataset.zip` | 11.4 MiB | IBM AMLSim example: accounts, alerts, and 1,323,234 synthetic transactions with `IS_FRAUD`. Useful for alert pipelines, graph analytics, and transaction-monitoring controls. | Kaggle says “Other”; description points to the upstream Apache-2.0 AMLSim project. Preserve upstream notices. | https://www.kaggle.com/datasets/anshankul/ibm-amlsim-example-dataset |
| Banking; AML | `downloads/huggingface/ibm-aml-hi-small-trans.csv.zip` | 86.9 MiB | 5,078,345 synthetic IBM AML transactions with an `Is Laundering` target. Useful for scale and extreme imbalance. | The Hugging Face mirror has no dataset card/license. Confirm IBM source terms before redistribution or commercial use. | https://huggingface.co/datasets/eexzzm/IBM-Transactions-for-Anti-Money-Laundering-HI-Small-Trans |
| Financial crime; text | `downloads/huggingface/financial-fraud-sec-filings.csv` | 207.8 MiB | 170 rows (85 fraud and 85 non-fraud companies according to the card), with long SEC-filing text and a fraud label. Useful for governance/reporting-fraud text classification; not banking loss data. | Apache-2.0 on the dataset card | https://huggingface.co/datasets/amitkedia/Financial-Fraud-Dataset |
| Banking; synthetic financial-crime workflow | `downloads/huggingface/enterprise-financial-crime-transactions-sample.csv` and `enterprise-financial-crime-emails-sample.csv` | 448 KiB + 2.6 MiB | Samples from a 310,396-record synthetic workflow joining transactions, emails, alerts, investigations, and SARs. Good for proof-of-concept workflow/control testing. | Apache-2.0 is stated, but the card also advertises commercial/extended access; all records are synthetic. | https://huggingface.co/datasets/Webopen2026/enterprise-financial-crime-ai-dataset |
| Insurance; claims fraud | `downloads/kaggle/vehicle-claim-fraud-detection.zip` | 348 KiB | 15,420 vehicle claims, 33 fields, `FraudFound_P` target. Useful for claims-control/fraud models; it is not insurer operational-loss data. | CC0 per Kaggle; provenance is thin | https://www.kaggle.com/datasets/shivamb/vehicle-claim-fraud-detection |
| Insurance; conduct/complaints | `downloads/kaggle/insurance-company-complaints-connecticut.zip` | 740 KiB | 38,267 Connecticut insurance complaints with company, coverage, reason, disposition, conclusion, recovery, and status. Useful for conduct risk, complaint handling, redress, and control indicators. | CC0 per Kaggle. Prefer the underlying Connecticut government portal when establishing production provenance. | https://www.kaggle.com/datasets/adelanseur/insurance-company-complaints |
| Reinsurance; legal/document controls | `downloads/huggingface/reinsurance-contracts-2001-2024.parquet` | 51.9 MiB | Default corpus: Hugging Face reports 992 SEC-filed documents. Text, SEC metadata, and labels for contract type/class. Labels were generated by three LLMs and reconciled; card prose says 990, so validate before use. Useful for document-processing controls, not loss severity/frequency. | CC BY 4.0 | https://huggingface.co/datasets/andreaaltomani/reinsurance-contracts-classification |
| Banking; conduct risk | `downloads/public-authorities/cfpb-consumer-complaints.csv.zip` | 337 MiB compressed / 5.18 GiB uncompressed | Complete CFPB complaint database as downloaded. Company/product/issue, response, timeliness, and optional narratives. Strong real-world conduct/process-failure signal, but not a representative sample and not direct loss amounts. The source updates generally daily. | Public U.S. government data; observe CFPB terms and narrative/privacy guidance | https://www.consumerfinance.gov/data-research/consumer-complaints/ |
| Banking and insurance; conduct risk | `downloads/public-authorities/fca-firm-level-complaints-2025-h2.xlsx` | 111 KiB | Latest available FCA firm-level H2 2025 workbook at research time. Covers banking/credit cards, insurance/pure protection, home finance, investments, and pensions. | UK public-sector information; check FCA reuse terms | https://www.fca.org.uk/data/complaints-data/previous-complaints-data |
| Banking and insurance; conduct risk | `downloads/public-authorities/fca-aggregate-complaints-2025-h2.xlsx` | 80 KiB | FCA aggregate complaints tables for H2 2025. Useful for trend and benchmark KRIs. | UK public-sector information; check FCA reuse terms | https://www.fca.org.uk/data/complaints-data/previous-complaints-data |
| Banking; regulatory exposure | `downloads/public-authorities/ecb-operational-risk-exposure-lsi.csv` | 9.4 KiB | 19 quarterly observations (2020-Q2 onward at download time) for ECB series `SUP.Q.B01.W0._Z.E6100._T.LSI._Z.ALL.LE.E.C`: BIA operational-risk exposure for less-significant institutions. Aggregate exposure, not losses. | ECB data reuse terms | https://data.ecb.europa.eu/data/datasets/SUP |
| Banking; regulatory aggregate | `downloads/public-authorities/central-bank-ireland-operational-risk-data-2024.pdf` | 344 KiB | One-page 2024 aggregate with operational-risk own-funds requirement shares and gross loss as a percentage of income. Reference data, not machine-ready event data. | Public regulator publication | https://www.centralbank.ie/docs/default-source/regulation/how-we-regulate/supervision/aggregate-statistical-data/gns-4-1-2-2-5-operational-risk-data.pdf |
| Banking; schema/taxonomy | `downloads/public-authorities/eba-op-risk-loss-template.xlsx` | 56 KiB | EBA consultation template for operational-risk loss reporting. Not observations, but valuable as a target schema/taxonomy for normalizing an internal or external event library. | Public consultation document; not a dataset | https://www.eba.europa.eu/publications-and-media/publications/consultation-module-operational-risk-losses |
| Cross-sector; cyber operational events | `downloads/cyber/vcdb-master.zip` | 55.1 MiB | VERIS Community Database repository snapshot; 10,596 validated/overridden JSON incident records in this archive. Filter NAICS 52 for Finance and Insurance. Public reports are selection-biased and the collection is historically health-sector-heavy. | CC BY-SA 4.0 in repository | https://github.com/vz-risk/VCDB |

## High-value datasets not downloaded

### Direct operational-loss/event sources

| Dataset | Sector | Why it matters | Access |
|---|---|---|---|
| ORX Global Banking Loss Data | Banking | Standardized, anonymized event-level consortium data; intended for benchmarking, scenario analysis, and modeling. | Membership only: https://orx.org/global-banking-loss-data-service |
| ORX Global Insurance Loss Data | Insurance/reinsurance | More than 13,000 insurer events and over EUR 8bn reported by ORX; the closest authoritative insurance operational-loss source found. | Membership only: https://orx.org/global-insurance-loss-data-service |
| ORX News | Banking, insurance, asset management | 11,000+ structured publicly reported loss-event digests, 50+ classification dimensions, API/CSV export. | Paid subscription; free trial/contact: https://orx.org/news |
| RiskBusiness GOLD | Primarily banking | Consortium operational-loss data contributed, quality-assured, and anonymized. | Participating firms/subscription: https://riskbusiness.com/gold/ |
| IBM FIRST Risk Case Studies | Financial services | 17,000+ external risk/loss case studies spanning fraud, governance, process errors, and market practices. | Commercial: https://www.ibm.com/products/first-risk-case-studies |
| SAS OpRisk Global Data | Cross-industry | Publicly reported losses over a stated threshold; used for external loss/scenario work. | Commercial; product access must be requested. Background: https://support.sas.com/resources/papers/proceedings20/5069-2020.pdf |

### Kaggle shortlist

| Dataset | Relevance | Why not downloaded |
|---|---|---|
| SAML-D | 9,504,852 synthetic AML transactions, 28 typologies, published-methodology provenance | About 996 MiB uncompressed; CC BY-NC-SA 4.0. https://www.kaggle.com/datasets/berkanoztas/synthetic-transaction-monitoring-dataset-aml |
| Bank Account Fraud (BAF), NeurIPS 2022 | Six realistic, biased, imbalanced, dynamic synthetic datasets; 1M rows each | About 1.36 GiB uncompressed; CC BY-NC-SA 4.0. https://www.kaggle.com/datasets/sgpjesus/bank-account-fraud-dataset-neurips-2022 |
| IBM Transactions for AML | Very large synthetic transaction graph collection | About 8.18 GiB; CDLA-Sharing 1.0. https://www.kaggle.com/datasets/ealtman2019/ibm-transactions-for-anti-money-laundering-aml |
| PaySim | 6.36M synthetic mobile-money transactions and fraud labels | Roughly 494 MiB CSV; common benchmark with known simulation limits. https://www.kaggle.com/datasets/ealaxi/paysim1 |
| Elliptic Bitcoin transaction graph | Real transaction graph with illicit/licit labels | About 306 MiB; CC BY-NC-ND 4.0 limits derivatives. https://www.kaggle.com/datasets/ellipticco/elliptic-data-set |
| IEEE-CIS Fraud Detection | Rich e-commerce transaction/identity fraud benchmark | Requires Kaggle account and acceptance of competition rules; non-commercial competition-data terms. https://www.kaggle.com/competitions/ieee-fraud-detection/data |
| Healthcare Provider Fraud | Insurance/claims fraud proxy | 26.6 MiB and CC0, but Medicare provider fraud is less directly relevant to insurer operational losses than the downloaded claims/complaints sets. https://www.kaggle.com/datasets/rohitrox/healthcare-provider-fraud-detection-analysis |
| Texas Insurance Complaints | Real public complaints, U.S. Government Works | About 87 MiB uncompressed; redundant with the smaller Connecticut and FCA complaint sets already downloaded. https://www.kaggle.com/datasets/chitreshkr/texas-insurance-complaints-dataset |

### Hugging Face shortlist

| Dataset | Relevance | Why not downloaded |
|---|---|---|
| Reinsurance Contracts, extended config | 6,726 SEC documents including amendments, endorsements, and references to reinsurance | 891 MiB Parquet / about 4.17 GiB decoded; core 992-document corpus was downloaded. https://huggingface.co/datasets/andreaaltomani/reinsurance-contracts-classification |
| FinSafeGuard | 709,303 synthetic BFSI safety conversations over 20 categories, including identity/fraud, AML, account takeover, and rogue trading | Useful for LLM control/governance testing rather than operational-loss analysis; CC BY-NC 4.0. https://huggingface.co/datasets/domyn/FinSafeGuard |
| DomynGuardFinancialBenchmark | 106,307 model predictions paired with financial-safety labels | Benchmark of a guardrail model, not institutional events. https://huggingface.co/datasets/domyn/DomynGuardFinancialBenchmark |
| FinanceMTEB FinancialFraud | 170 long SEC-filing documents for fraud classification | About 81 MiB download and no license stated in the card; overlaps the Apache-licensed SEC corpus downloaded above. https://huggingface.co/datasets/FinanceMTEB/FinancialFraud |
| Enterprise Financial Crime AI Dataset, full | 310,396 synthetic transaction/email/alert/investigation/SAR records | Full set is about 339 MiB; smaller samples were downloaded to evaluate schema and fitness first. https://huggingface.co/datasets/Webopen2026/enterprise-financial-crime-ai-dataset |
| Insurance fraud-flag coherence risk | Small insurance fraud triage benchmark | Marked `pre_release`; synthetic/benchmark use only. https://huggingface.co/datasets/ClarusC64/insurance-fraud-flag-coherence-risk-v0.1 |

### Authoritative public sources worth connecting

| Dataset | Operational-risk use | Access/link |
|---|---|---|
| FDIC Enforcement Decisions and Orders | Compliance, governance, AML, unsafe-practice, and legal-risk event discovery. Search interface supports CSV export but no stable bulk-download URL was exposed. | https://orders.fdic.gov/s/searchform |
| OpenFEMA NFIP Redacted Claims | More than 2M real claim transactions; useful for insurer/reinsurer claims operations and catastrophe/claims benchmarks. Primarily underwriting/catastrophe risk, not the carrier's operational loss. | API/download: https://www.fema.gov/api/open/v2/FimaNfipClaims |
| NOAA Storm Events | U.S. weather events, deaths, injuries, property/crop damage, and narratives from 1950 onward. Useful for business continuity and reinsurance scenario inputs; not operational losses by itself. | Bulk CSV index: https://www.ncei.noaa.gov/pub/data/swdi/stormevents/csvfiles/ |
| EM-DAT | Global disaster impacts by event and country, updated weekly. Useful for external-event and reinsurance scenarios. | Free non-commercial access after registration; XLSX download: https://public.emdat.be/ |
| OFAC Sanctions List Service | Authoritative sanctions/KYC control input, not loss data. | https://sanctionslist.ofac.treas.gov/Home/SdnList |

## Recommended use by objective

| Objective | Start with | Do not infer |
|---|---|---|
| External-loss severity/frequency calibration | ORX, GOLD, IBM FIRST, or SAS plus internal losses | Do not calibrate capital from Kaggle synthetic data or complaint counts. |
| Scenario library | ORX News/IBM FIRST, VCDB filtered to NAICS 52, FDIC orders, internal near misses | Public reports are selection-biased and loss amounts are often missing. |
| Fraud/AML control testing | IBM AMLSim, IBM AML HI-Small, SAML-D/BAF if licenses fit | Synthetic detection performance is not production performance. |
| Conduct/customer-outcome KRIs | CFPB, FCA, Connecticut complaints | Raw counts require exposure denominators and reporting-regime controls. |
| Insurance claims fraud | Vehicle claims fraud plus government complaint data | Claims fraud is an operational-risk driver/proxy, not the insurer's complete op-risk profile. |
| Reinsurance document controls | Downloaded SEC contract corpus | LLM-generated labels require manual validation; this is not a claims/loss dataset. |
| Cyber operational risk | VCDB NAICS 52 plus internal incident data | VCDB is not comprehensive or representative; it is biased by public disclosure regimes. |
| Business continuity/cat scenarios | NOAA, FEMA NFIP, EM-DAT | Catastrophe losses belong primarily to underwriting/insurance risk unless the analysis explicitly models operational disruption. |

## Refresh notes

- CFPB, FCA, ECB, VCDB, FDIC, NOAA, FEMA, ORX, Kaggle, and Hugging Face content can change. Record retrieval dates and hashes in downstream work.
- Kaggle and Hugging Face are hosting platforms, not proof of provenance. Verify upstream creators, methodology, and license before production or commercial use.
- Do not combine synthetic and real observations without a source flag.
- Normalize event types to one controlled taxonomy (for example, the EBA template or ORX reporting standards) before cross-source analysis.
- Keep event date, discovery date, accounting date, gross loss, recoveries, net loss, currency, organization size/exposure, source type, and confidence separate when building a loss-event mart.
